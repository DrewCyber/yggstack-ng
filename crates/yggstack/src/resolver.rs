/// DNS + `.pk.ygg` name resolver using the Yggdrasil netstack.
use std::net::Ipv6Addr;
use std::sync::Arc;

use yggdrasil::address::addr_for_key;

use crate::netstack::YggNetstack;

pub const NAME_MAPPING_SUFFIX: &str = ".pk.ygg";

pub struct NameResolver {
    netstack: Arc<YggNetstack>,
    /// Nameserver address, e.g. `"[200:peer::dns:addr]:53"`.
    nameserver: Option<String>,
}

impl NameResolver {
    pub fn new(netstack: Arc<YggNetstack>, nameserver: &str) -> Self {
        Self {
            netstack,
            nameserver: if nameserver.is_empty() {
                None
            } else {
                Some(nameserver.to_string())
            },
        }
    }

    /// Resolve a hostname or `.pk.ygg` name to an IPv6 address.
    pub async fn resolve(&self, name: &str) -> Result<Ipv6Addr, String> {
        // 1. Direct IP literal?
        if let Ok(ip) = name.parse::<Ipv6Addr>() {
            return Ok(ip);
        }

        // 2. `.pk.ygg` suffix → derive from public key.
        if name.ends_with(NAME_MAPPING_SUFFIX) {
            let stripped = name
                .trim_end_matches(NAME_MAPPING_SUFFIX)
                .rsplit('.')
                .next()
                .unwrap_or("");
            let bytes = hex::decode(stripped)
                .map_err(|e| format!("hex decode: {}", e))?;
            if bytes.len() != 32 {
                return Err(format!("public key must be 32 bytes, got {}", bytes.len()));
            }
            let mut pk = [0u8; 32];
            pk.copy_from_slice(&bytes);
            let addr = addr_for_key(&pk);
            return Ok(Ipv6Addr::from(addr.0));
        }

        // 3. Forward DNS query via nameserver through netstack.
        let ns = self
            .nameserver
            .as_deref()
            .ok_or_else(|| format!("no nameserver configured; cannot resolve '{}'", name))?;

        self.dns_lookup_ipv6(name, ns).await
    }

    async fn dns_lookup_ipv6(&self, name: &str, nameserver: &str) -> Result<Ipv6Addr, String> {
        // Parse nameserver as "[addr]:port" or "addr:port".
        let ns_addr: std::net::SocketAddr = nameserver
            .parse()
            .or_else(|_| format!("[{}]:53", nameserver).parse())
            .map_err(|_| format!("invalid nameserver address '{}'", nameserver))?;

        let query = build_dns_query(name, 28 /* AAAA */);

        // Try UDP first (DNS default transport). The first attempt may be
        // buffered while a DHT route lookup happens; if it times out we retry
        // once (the route should be established by then) before falling back
        // to TCP for truncated responses.
        let udp_result = match self.dns_lookup_udp(&query, ns_addr, 10).await {
            Ok(resp) => Ok(resp),
            Err(e) => {
                tracing::debug!("DNS UDP attempt 1 failed for '{}': {}; retrying", name, e);
                // Retry: route should be established now.
                self.dns_lookup_udp(&query, ns_addr, 10).await
            }
        };

        match udp_result {
            Ok(resp) => {
                // Check TC (truncated) bit in flags byte 2 bit 1.
                let truncated = resp.len() >= 3 && (resp[2] & 0x02) != 0;
                if truncated {
                    tracing::debug!("DNS UDP response truncated for '{}', retrying via TCP", name);
                    return self.dns_lookup_tcp(&query, ns_addr).await
                        .and_then(|r| parse_dns_aaaa_response(&r)
                            .ok_or_else(|| format!("no AAAA record found for '{}'", name)));
                }
                parse_dns_aaaa_response(&resp)
                    .ok_or_else(|| format!("no AAAA record found for '{}'", name))
            }
            Err(e) => {
                tracing::debug!("DNS UDP failed for '{}': {}; trying TCP", name, e);
                self.dns_lookup_tcp(&query, ns_addr).await
                    .and_then(|r| parse_dns_aaaa_response(&r)
                        .ok_or_else(|| format!("no AAAA record found for '{}'", name)))
            }
        }
    }

    async fn dns_lookup_udp(
        &self,
        query: &[u8],
        ns_addr: std::net::SocketAddr,
        timeout_secs: u64,
    ) -> Result<Vec<u8>, String> {
        let udp = self
            .netstack
            .open_udp()
            .map_err(|e| format!("UDP open: {}", e))?;

        udp.send_to(query, ns_addr)
            .await
            .map_err(|e| format!("DNS UDP send: {}", e))?;

        let mut buf = vec![0u8; 4096];
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let (n, _) = tokio::time::timeout(timeout, udp.recv_from(&mut buf))
            .await
            .map_err(|_| "DNS UDP timeout".to_string())?
            .map_err(|e| format!("DNS UDP recv: {}", e))?;

        Ok(buf[..n].to_vec())
    }

    async fn dns_lookup_tcp(
        &self,
        query: &[u8],
        ns_addr: std::net::SocketAddr,
    ) -> Result<Vec<u8>, String> {
        // DNS-over-TCP uses a 2-byte length prefix.
        let mut framed = Vec::with_capacity(2 + query.len());
        framed.extend_from_slice(&(query.len() as u16).to_be_bytes());
        framed.extend_from_slice(query);

        let mut stream = self
            .netstack
            .dial_tcp(ns_addr)
            .await
            .map_err(|e| format!("DNS connect: {}", e))?;

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(&framed)
            .await
            .map_err(|e| format!("DNS write: {}", e))?;

        // Read response (2-byte length prefix).
        let mut len_buf = [0u8; 2];
        stream
            .read_exact(&mut len_buf)
            .await
            .map_err(|e| format!("DNS read len: {}", e))?;
        let resp_len = u16::from_be_bytes(len_buf) as usize;

        let mut resp = vec![0u8; resp_len];
        stream
            .read_exact(&mut resp)
            .await
            .map_err(|e| format!("DNS read body: {}", e))?;

        Ok(resp)
    }
}

// ── Minimal DNS wire format ───────────────────────────────────────────────────

fn build_dns_query(name: &str, qtype: u16) -> Vec<u8> {
    let mut msg = Vec::with_capacity(64);

    // Header: ID=1, QR=0(query), OPCODE=0, RD=1, 1 question
    msg.extend_from_slice(&[0x00, 0x01]); // ID
    msg.extend_from_slice(&[0x01, 0x00]); // Flags: RD=1
    msg.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    msg.extend_from_slice(&[0x00, 0x00]); // ANCOUNT=0
    msg.extend_from_slice(&[0x00, 0x00]); // NSCOUNT=0
    msg.extend_from_slice(&[0x00, 0x00]); // ARCOUNT=0

    // Question: encode name as DNS labels
    for label in name.trim_end_matches('.').split('.') {
        msg.push(label.len() as u8);
        msg.extend_from_slice(label.as_bytes());
    }
    msg.push(0x00); // root label

    msg.extend_from_slice(&qtype.to_be_bytes()); // QTYPE
    msg.extend_from_slice(&[0x00, 0x01]); // QCLASS=IN

    msg
}

fn parse_dns_aaaa_response(buf: &[u8]) -> Option<Ipv6Addr> {
    if buf.len() < 12 {
        return None;
    }
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;
    if ancount == 0 {
        return None;
    }

    // Skip the question section.
    let mut pos = 12;

    // Skip QDCOUNT questions (1 question).
    pos = skip_dns_name(buf, pos)?;
    pos += 4; // QTYPE + QCLASS

    // Parse answer RRs.
    for _ in 0..ancount {
        pos = skip_dns_name(buf, pos)?;
        if pos + 10 > buf.len() {
            return None;
        }
        let rtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
        let _rclass = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);
        let _ttl = u32::from_be_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]]);
        let rdlength = u16::from_be_bytes([buf[pos + 8], buf[pos + 9]]) as usize;
        pos += 10;

        if rtype == 28 && rdlength == 16 && pos + 16 <= buf.len() {
            // AAAA record
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[pos..pos + 16]);
            return Some(Ipv6Addr::from(octets));
        }
        pos += rdlength;
    }
    None
}

fn skip_dns_name(buf: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        if pos >= buf.len() {
            return None;
        }
        let len = buf[pos];
        if len == 0 {
            return Some(pos + 1);
        }
        if len & 0xC0 == 0xC0 {
            // Pointer
            return Some(pos + 2);
        }
        pos += 1 + len as usize;
    }
}
