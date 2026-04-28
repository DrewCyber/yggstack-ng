/// SOCKS5 proxy server using the Yggdrasil netstack.
///
/// Only the CONNECT command is supported (TCP proxying).
/// Authentication: none (method 0x00).
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::sync::Arc;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::netstack::YggNetstack;
use crate::resolver::NameResolver;

// SOCKS5 constants
const SOCKS5_VERSION: u8 = 5;
const AUTH_NO_AUTH: u8 = 0x00;
const AUTH_NO_ACCEPTABLE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
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

    pub async fn serve_tcp(self: Arc<Self>, addr: &str, stop_tx: broadcast::Sender<()>) -> io::Result<()> {
        let listener = TcpListener::bind(addr).await?;
        tracing::info!("SOCKS5 server listening on {}", addr);
        let mut stop = stop_tx.subscribe();
        loop {
            tokio::select! {
                _ = stop.recv() => {
                    tracing::info!("SOCKS5: stopped on {}", addr);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((stream, peer)) => {
                            tracing::debug!("SOCKS5 connection from {}", peer);
                            let srv = self.clone();
                            let stop_conn = stop_tx.subscribe();
                            tokio::spawn(async move {
                                if let Err(e) = srv.handle_client(stream, stop_conn).await {
                                    tracing::debug!("SOCKS5 client error: {}", e);
                                }
                            });
                        }
                        Err(e) => tracing::warn!("SOCKS5 accept error: {}", e),
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_client(&self, mut client: TcpStream, mut stop: broadcast::Receiver<()>) -> io::Result<()> {
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

        if cmd != CMD_CONNECT {
            // Only CONNECT is supported
            send_reply(&mut client, REP_GENERAL_FAILURE, None).await?;
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported CMD {}", cmd),
            ));
        }

        let dest_addr = match read_address(&mut client, atyp).await {
            Ok(a) => a,
            Err(e) => {
                send_reply(&mut client, REP_ADDR_TYPE_UNSUPPORTED, None).await?;
                return Err(e);
            }
        };
        let dest_port = client.read_u16().await?;

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
            _ = stop.recv() => {
                tracing::debug!("SOCKS5 relay cancelled for {}", remote_addr);
            }
            r = tokio::io::copy(&mut cr, &mut yw) => {
                tracing::debug!("SOCKS5 client→ygg done: {:?}", r);
            }
            r = tokio::io::copy(&mut yr, &mut cw) => {
                tracing::debug!("SOCKS5 ygg→client done: {:?}", r);
            }
        }

        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
