/// Minimal SOCKS5 CONNECT server.
///
/// Handles no-auth (method 0x00) and the CONNECT command.
/// Domain names are resolved via a Yggdrasil DNS server (nameserver) when
/// configured, which allows resolving `.ygg` hostnames.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{self, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::netstack::Netstack;

/// Start a SOCKS5 server on `listen` (TCP address).
/// `nameserver`: optional Yggdrasil DNS resolver for domain lookups.
pub async fn serve_socks5(
    listen: &str,
    netstack: Arc<Netstack>,
    nameserver: Option<SocketAddr>,
) -> Result<(), String> {
    let listener = TcpListener::bind(listen)
        .await
        .map_err(|e| format!("socks5 bind {}: {}", listen, e))?;
    if let Some(ns) = nameserver {
        tracing::info!("SOCKS5 server listening on {} (nameserver: {})", listen, ns);
    } else {
        tracing::info!("SOCKS5 server listening on {}", listen);
    }

    loop {
        let (stream, peer) = listener
            .accept()
            .await
            .map_err(|e| format!("socks5 accept: {}", e))?;
        tracing::debug!("SOCKS5 new connection from {}", peer);
        let ns = netstack.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_socks5(stream, ns, nameserver).await {
                tracing::debug!("SOCKS5 connection error ({}): {}", peer, e);
            }
        });
    }
}

async fn handle_socks5(
    mut client: TcpStream,
    netstack: Arc<Netstack>,
    nameserver: Option<SocketAddr>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // ── Greeting ──────────────────────────────────────────────────────────────
    let ver = client.read_u8().await?;
    if ver != 5 {
        return Err(format!("not SOCKS5 (ver={})", ver).into());
    }
    let n_methods = client.read_u8().await? as usize;
    let mut methods = vec![0u8; n_methods];
    client.read_exact(&mut methods).await?;

    if !methods.contains(&0x00) {
        client.write_all(&[5, 0xFF]).await?;
        return Err("no acceptable auth method".into());
    }
    client.write_all(&[5, 0x00]).await?;

    // ── Request ───────────────────────────────────────────────────────────────
    let _ver = client.read_u8().await?;  // 5
    let cmd  = client.read_u8().await?;  // 1=CONNECT
    let _rsv = client.read_u8().await?;  // 0
    let atyp = client.read_u8().await?;

    if cmd != 1 {
        client.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
        return Err(format!("unsupported command {}", cmd).into());
    }

    let target: SocketAddr = match atyp {
        1 => {
            // IPv4
            let mut a = [0u8; 4];
            client.read_exact(&mut a).await?;
            let port = client.read_u16().await?;
            SocketAddr::new(IpAddr::V4(Ipv4Addr::from(a)), port)
        }
        3 => {
            // Domain name
            let len = client.read_u8().await? as usize;
            let mut domain_bytes = vec![0u8; len];
            client.read_exact(&mut domain_bytes).await?;
            let domain = String::from_utf8(domain_bytes)?;
            let port = client.read_u16().await?;

            // Fast path: direct IP literal
            if let Ok(ip) = domain.parse::<IpAddr>() {
                SocketAddr::new(ip, port)
            } else if let Some(ns_addr) = nameserver {
                // Resolve via Yggdrasil DNS server
                tracing::debug!("SOCKS5 resolving '{}' via {}", domain, ns_addr);
                match dns_resolve_aaaa(&netstack, ns_addr, &domain).await {
                    Ok(ip6) => {
                        tracing::debug!("SOCKS5 '{}' → {}", domain, ip6);
                        SocketAddr::new(IpAddr::V6(ip6), port)
                    }
                    Err(e) => {
                        tracing::debug!("SOCKS5 DNS failed for '{}': {}", domain, e);
                        client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
                        return Err(e);
                    }
                }
            } else {
                // Fall back to system resolver
                tokio::net::lookup_host(format!("{}:{}", domain, port))
                    .await?
                    .next()
                    .ok_or_else(|| format!("cannot resolve '{}'", domain))?
            }
        }
        4 => {
            // IPv6
            let mut a = [0u8; 16];
            client.read_exact(&mut a).await?;
            let port = client.read_u16().await?;
            SocketAddr::new(IpAddr::V6(Ipv6Addr::from(a)), port)
        }
        _ => {
            client.write_all(&[5, 8, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(format!("unsupported ATYP {}", atyp).into());
        }
    };

    // ── Dial through Yggdrasil netstack ───────────────────────────────────────
    let mut ygg_conn = match netstack.dial_tcp(target).await {
        Ok(c) => c,
        Err(e) => {
            client.write_all(&[5, 4, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(format!("dial {}: {}", target, e).into());
        }
    };

    // Success reply: bound addr 0.0.0.0:0
    client.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;

    // ── Proxy ─────────────────────────────────────────────────────────────────
    let (mut cr, mut cw) = client.split();
    let (mut yr, mut yw) = tokio::io::split(&mut ygg_conn);

    tokio::select! {
        r = io::copy(&mut cr, &mut yw) => { r?; }
        r = io::copy(&mut yr, &mut cw) => { r?; }
    }
    Ok(())
}

// ── DNS-over-Yggdrasil ────────────────────────────────────────────────────────

/// Resolve `domain` to an IPv6 address using a DNS AAAA query sent via the
/// Yggdrasil netstack UDP to `nameserver`.
async fn dns_resolve_aaaa(
    netstack: &Netstack,
    nameserver: SocketAddr,
    domain: &str,
) -> Result<Ipv6Addr, Box<dyn std::error::Error + Send + Sync>> {
    use rand::Rng;
    let id: u16 = rand::thread_rng().gen();
    // Use a random high port so smoltcp can receive the response.
    let local_port: u16 = rand::thread_rng().gen_range(50000..60000);

    let query = dns_build_query(id, domain)?;

    let mut sock = netstack
        .dial_udp(local_port)
        .await
        .map_err(|e| format!("DNS UDP open: {}", e))?;

    sock.try_send(Bytes::from(query), nameserver)
        .map_err(|e| format!("DNS UDP send: {}", e))?;

    let (data, _from) = tokio::time::timeout(Duration::from_secs(5), sock.recv())
        .await
        .map_err(|_| "DNS query timed out")?
        .ok_or("DNS socket closed")?;

    dns_parse_aaaa(&data, id)
}

/// Build a minimal DNS AAAA query packet.
fn dns_build_query(id: u16, domain: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(&id.to_be_bytes());   // Transaction ID
    buf.extend_from_slice(&[0x01, 0x00]);        // Flags: RD=1
    buf.extend_from_slice(&[0x00, 0x01]);        // QDCOUNT=1
    buf.extend_from_slice(&[0x00, 0x00]);        // ANCOUNT=0
    buf.extend_from_slice(&[0x00, 0x00]);        // NSCOUNT=0
    buf.extend_from_slice(&[0x00, 0x00]);        // ARCOUNT=0
    // QNAME: length-prefixed labels
    for label in domain.split('.') {
        let b = label.as_bytes();
        if b.len() > 63 {
            return Err("DNS label too long".into());
        }
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    buf.push(0x00);                              // root label
    buf.extend_from_slice(&[0x00, 0x1C]);        // QTYPE=AAAA (28)
    buf.extend_from_slice(&[0x00, 0x01]);        // QCLASS=IN
    Ok(buf)
}

/// Parse the first AAAA record from a DNS response.
fn dns_parse_aaaa(
    buf: &[u8],
    expected_id: u16,
) -> Result<Ipv6Addr, Box<dyn std::error::Error + Send + Sync>> {
    if buf.len() < 12 {
        return Err("DNS response too short".into());
    }
    let resp_id = u16::from_be_bytes([buf[0], buf[1]]);
    if resp_id != expected_id {
        return Err(format!("DNS ID mismatch ({} != {})", resp_id, expected_id).into());
    }
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let rcode = flags & 0x000F;
    if rcode != 0 {
        return Err(format!("DNS error RCODE={}", rcode).into());
    }
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]) as usize;
    let ancount = u16::from_be_bytes([buf[6], buf[7]]) as usize;

    let mut pos = 12;
    // Skip question section
    for _ in 0..qdcount {
        pos = dns_skip_name(buf, pos)?;
        pos += 4; // QTYPE + QCLASS
    }
    // Walk answer records looking for AAAA
    for _ in 0..ancount {
        pos = dns_skip_name(buf, pos)?;
        if pos + 10 > buf.len() {
            break;
        }
        let rtype  = u16::from_be_bytes([buf[pos],   buf[pos+1]]);
        let rdlen  = u16::from_be_bytes([buf[pos+8], buf[pos+9]]) as usize;
        pos += 10;
        if rtype == 28 && rdlen == 16 && pos + 16 <= buf.len() {
            let mut addr = [0u8; 16];
            addr.copy_from_slice(&buf[pos..pos + 16]);
            return Ok(Ipv6Addr::from(addr));
        }
        pos += rdlen;
    }
    Err(format!("no AAAA record in DNS response for (ancount={})", ancount).into())
}

/// Skip a DNS name (handles pointer compression) and return the position after it.
fn dns_skip_name(
    buf: &[u8],
    mut pos: usize,
) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
    loop {
        if pos >= buf.len() {
            return Err("DNS name parse: out of bounds".into());
        }
        let b = buf[pos];
        if b == 0x00 {
            return Ok(pos + 1);
        }
        if b & 0xC0 == 0xC0 {
            // Pointer: 2 bytes
            return Ok(pos + 2);
        }
        pos += (b as usize) + 1;
    }
}
