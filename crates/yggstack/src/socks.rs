/// SOCKS5 proxy server using the Yggdrasil netstack.
///
/// Supported commands: CONNECT (TCP) and UDP ASSOCIATE.
/// Authentication: none (method 0x00).
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket as TokioUdpSocket};

use crate::netstack::YggNetstack;
use crate::resolver::NameResolver;

// SOCKS5 constants
const SOCKS5_VERSION: u8 = 5;
const AUTH_NO_AUTH: u8 = 0x00;
const AUTH_NO_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const CMD_UDP_ASSOCIATE: u8 = 0x03;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;
const REP_SUCCESS: u8 = 0x00;
const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CONN_REFUSED: u8 = 0x05;
const REP_ADDR_TYPE_UNSUPPORTED: u8 = 0x08;

pub struct Socks5Server {
    netstack: Arc<YggNetstack>,
    resolver: Arc<NameResolver>,
}

impl Socks5Server {
    pub fn new(netstack: Arc<YggNetstack>, resolver: Arc<NameResolver>) -> Self {
        Self { netstack, resolver }
    }

    pub async fn serve_tcp(self: Arc<Self>, addr: &str) -> io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!("SOCKS5 server listening on {}", addr);
        loop {
            match listener.accept().await {
                Ok((stream, peer)) => {
                    tracing::debug!("SOCKS5 connection from {}", peer);
                    let srv = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = srv.handle_client(stream).await {
                            tracing::debug!("SOCKS5 client error: {}", e);
                        }
                    });
                }
                Err(e) => tracing::warn!("SOCKS5 accept error: {}", e),
            }
        }
    }

    async fn handle_client(&self, mut client: TcpStream) -> io::Result<()> {
        // Phase 1: negotiation
        let ver = client.read_u8().await?;
        if ver != SOCKS5_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not SOCKS5"));
        }
        let nmethods = client.read_u8().await?;
        let mut methods = vec![0u8; nmethods as usize];
        client.read_exact(&mut methods).await?;

        if methods.contains(&AUTH_NO_AUTH) {
            client.write_all(&[SOCKS5_VERSION, AUTH_NO_AUTH]).await?;
        } else {
            client
                .write_all(&[SOCKS5_VERSION, AUTH_NO_ACCEPTABLE])
                .await?;
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "no acceptable auth",
            ));
        }

        // Phase 2: request
        let ver = client.read_u8().await?;
        if ver != SOCKS5_VERSION {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad version"));
        }
        let cmd = client.read_u8().await?;
        let _rsv = client.read_u8().await?;
        let atyp = client.read_u8().await?;

        let dest_addr = match read_address(&mut client, atyp).await {
            Ok(a) => a,
            Err(e) => {
                send_reply(&mut client, REP_ADDR_TYPE_UNSUPPORTED, None).await?;
                return Err(e);
            }
        };
        let dest_port = client.read_u16().await?;

        match cmd {
            CMD_CONNECT => self.handle_connect(client, dest_addr, dest_port).await,
            CMD_UDP_ASSOCIATE => self.handle_udp_associate(client).await,
            _ => {
                send_reply(&mut client, REP_GENERAL_FAILURE, None).await?;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("unsupported CMD {}", cmd),
                ))
            }
        }
    }

    async fn handle_connect(
        &self,
        mut client: TcpStream,
        dest_addr: Destination,
        dest_port: u16,
    ) -> io::Result<()> {
        // Resolve hostname to an IPv6 address.
        let remote_addr: SocketAddr = match dest_addr {
            Destination::Ip(ip) => SocketAddr::new(ip, dest_port),
            Destination::Domain(name) => {
                tracing::debug!("SOCKS5 resolving '{}'", name);
                match self.resolver.resolve(&name).await {
                    Ok(ip6) => {
                        tracing::debug!("SOCKS5 resolved '{}' → {}", name, ip6);
                        SocketAddr::V6(SocketAddrV6::new(ip6, dest_port, 0, 0))
                    }
                    Err(e) => {
                        tracing::debug!("SOCKS5 resolve '{}' failed: {}", name, e);
                        send_reply(&mut client, REP_GENERAL_FAILURE, None).await?;
                        return Err(io::Error::other(e));
                    }
                }
            }
        };

        tracing::debug!("SOCKS5 dialing {}", remote_addr);
        // Dial via netstack.
        let ygg_stream = match self.netstack.dial_tcp(remote_addr).await {
            Ok(s) => {
                tracing::debug!("SOCKS5 connected to {}", remote_addr);
                s
            }
            Err(e) => {
                let rep = if e.kind() == io::ErrorKind::ConnectionRefused {
                    REP_CONN_REFUSED
                } else {
                    REP_GENERAL_FAILURE
                };
                send_reply(&mut client, rep, None).await?;
                return Err(e);
            }
        };

        send_reply(&mut client, REP_SUCCESS, Some(remote_addr)).await?;
        tracing::debug!("SOCKS5 relaying data for {}", remote_addr);

        // Relay data bidirectionally.
        let (mut cr, mut cw) = client.into_split();
        let (mut yr, mut yw) = tokio::io::split(ygg_stream);

        tokio::select! {
            r = tokio::io::copy(&mut cr, &mut yw) => {
                tracing::debug!("SOCKS5 client→ygg done: {:?}", r);
            }
            r = tokio::io::copy(&mut yr, &mut cw) => {
                tracing::debug!("SOCKS5 ygg→client done: {:?}", r);
            }
        }

        Ok(())
    }

    async fn handle_udp_associate(&self, mut client: TcpStream) -> io::Result<()> {
        // Bind an OS UDP socket for the relay. Use the same IP as the TCP
        // connection so the client can reach it.
        let local_tcp_addr = client.local_addr()?;
        let relay_bind = SocketAddr::new(local_tcp_addr.ip(), 0); // port 0 = OS picks
        let relay = TokioUdpSocket::bind(relay_bind).await?;
        let relay_addr = relay.local_addr()?;

        tracing::debug!("SOCKS5 UDP ASSOCIATE relay on {}", relay_addr);
        send_reply(&mut client, REP_SUCCESS, Some(relay_addr)).await?;

        let relay = Arc::new(relay);
        let netstack = self.netstack.clone();

        // Spawn the UDP relay loop. It runs until the TCP control channel closes.
        let relay_clone = relay.clone();
        let relay_task = tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            loop {
                let (n, client_addr) = match relay_clone.recv_from(&mut buf).await {
                    Ok(r) => r,
                    Err(_) => break,
                };
                if n < 4 {
                    continue; // too short for SOCKS5 UDP header
                }

                // Parse SOCKS5 UDP header: RSV(2) + FRAG(1) + ATYP(1) + ADDR + PORT
                let frag = buf[2];
                if frag != 0 {
                    continue; // fragmentation not supported
                }
                let atyp = buf[3];
                let (dest, hdr_len) = match parse_udp_header(&buf[..n], atyp) {
                    Some(r) => r,
                    None => continue,
                };

                let payload = &buf[hdr_len..n];

                // Send through Yggdrasil netstack (one per packet — TODO: reuse)
                tracing::debug!("UDP ASSOCIATE: opening udp socket for {:?}", dest);
                let ygg_udp = match netstack.open_udp() {
                    Ok(u) => u,
                    Err(e) => {
                        tracing::warn!("UDP ASSOCIATE open_udp failed: {}", e);
                        continue;
                    }
                };
                if let Err(e) = ygg_udp.send_to(payload, dest).await {
                    tracing::debug!("UDP ASSOCIATE send_to {} failed: {}", dest, e);
                    continue;
                }

                // Receive response with timeout
                let mut resp_buf = [0u8; 4096];
                let recv_result = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    ygg_udp.recv_from(&mut resp_buf),
                )
                .await;

                match recv_result {
                    Ok(Ok((resp_n, resp_addr))) => {
                        // Build SOCKS5 UDP response header + payload
                        let resp_packet = build_udp_header(resp_addr, &resp_buf[..resp_n]);
                        let _ = relay_clone.send_to(&resp_packet, client_addr).await;
                    }
                    Ok(Err(e)) => {
                        tracing::debug!("UDP ASSOCIATE recv failed: {}", e);
                    }
                    Err(_) => {
                        tracing::debug!("UDP ASSOCIATE recv timeout");
                    }
                }
            }
        });

        // Keep TCP control channel open. When it closes, stop the relay.
        let mut discard = [0u8; 1];
        let _ = client.read(&mut discard).await; // blocks until EOF/error
        tracing::debug!("SOCKS5 UDP ASSOCIATE control channel closed, aborting relay");
        relay_task.abort();
        // ygg_udp sockets created inside relay_task should be dropped when task aborts
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Parse SOCKS5 UDP request header, return (destination, header_length).
fn parse_udp_header(buf: &[u8], atyp: u8) -> Option<(SocketAddr, usize)> {
    match atyp {
        ATYP_IPV4 => {
            if buf.len() < 10 {
                return None;
            }
            let ip = Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]);
            let port = u16::from_be_bytes([buf[8], buf[9]]);
            Some((SocketAddr::new(std::net::IpAddr::V4(ip), port), 10))
        }
        ATYP_IPV6 => {
            if buf.len() < 22 {
                return None;
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&buf[4..20]);
            let ip = Ipv6Addr::from(octets);
            let port = u16::from_be_bytes([buf[20], buf[21]]);
            Some((SocketAddr::new(std::net::IpAddr::V6(ip), port), 22))
        }
        ATYP_DOMAIN => {
            if buf.len() < 5 {
                return None;
            }
            let dlen = buf[4] as usize;
            if buf.len() < 5 + dlen + 2 {
                return None;
            }
            // Domain not supported in UDP relay — would need async DNS
            None
        }
        _ => None,
    }
}

/// Build SOCKS5 UDP response header + payload.
fn build_udp_header(src: SocketAddr, payload: &[u8]) -> Vec<u8> {
    let mut pkt = Vec::with_capacity(22 + payload.len());
    pkt.extend_from_slice(&[0x00, 0x00, 0x00]); // RSV + FRAG
    match src {
        SocketAddr::V4(a) => {
            pkt.push(ATYP_IPV4);
            pkt.extend_from_slice(&a.ip().octets());
            pkt.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            pkt.push(ATYP_IPV6);
            pkt.extend_from_slice(&a.ip().octets());
            pkt.extend_from_slice(&a.port().to_be_bytes());
        }
    }
    pkt.extend_from_slice(payload);
    pkt
}

enum Destination {
    Ip(std::net::IpAddr),
    Domain(String),
}

async fn read_address<R: AsyncRead + Unpin>(
    r: &mut R,
    atyp: u8,
) -> io::Result<Destination> {
    match atyp {
        ATYP_IPV4 => {
            let mut buf = [0u8; 4];
            r.read_exact(&mut buf).await?;
            Ok(Destination::Ip(std::net::IpAddr::V4(Ipv4Addr::from(buf))))
        }
        ATYP_IPV6 => {
            let mut buf = [0u8; 16];
            r.read_exact(&mut buf).await?;
            Ok(Destination::Ip(std::net::IpAddr::V6(Ipv6Addr::from(buf))))
        }
        ATYP_DOMAIN => {
            let len = r.read_u8().await? as usize;
            let mut buf = vec![0u8; len];
            r.read_exact(&mut buf).await?;
            let name = String::from_utf8(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid domain"))?;
            Ok(Destination::Domain(name))
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown ATYP {}", atyp),
        )),
    }
}

async fn send_reply<W: AsyncWrite + Unpin>(
    w: &mut W,
    rep: u8,
    bound: Option<SocketAddr>,
) -> io::Result<()> {
    let mut buf = Vec::with_capacity(22);
    buf.push(SOCKS5_VERSION);
    buf.push(rep);
    buf.push(0x00); // RSV

    match bound {
        Some(SocketAddr::V4(a)) => {
            buf.push(ATYP_IPV4);
            buf.extend_from_slice(&a.ip().octets());
            buf.extend_from_slice(&a.port().to_be_bytes());
        }
        Some(SocketAddr::V6(a)) => {
            buf.push(ATYP_IPV6);
            buf.extend_from_slice(&a.ip().octets());
            buf.extend_from_slice(&a.port().to_be_bytes());
        }
        None => {
            // Return an IPv6 zero address.
            buf.push(ATYP_IPV6);
            buf.extend_from_slice(&[0u8; 16]);
            buf.extend_from_slice(&[0u8; 2]);
        }
    }
    w.write_all(&buf).await
}
