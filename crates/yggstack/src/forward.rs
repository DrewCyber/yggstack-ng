/// TCP and UDP port forwarding (local ↔ Yggdrasil and remote ↔ local).
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::io;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::Mutex;

use crate::netstack::{Netstack, NetTcpStream};

// ── Local TCP: local port → Yggdrasil remote ──────────────────────────────────

/// Listen on `listen_addr` (local) and forward each TCP connection to `remote`
/// on the Yggdrasil netstack (like `ssh -L`).
pub async fn local_tcp(
    listen_addr: SocketAddr,
    remote: SocketAddr,
    netstack: Arc<Netstack>,
) -> Result<(), String> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .map_err(|e| format!("local-tcp bind {}: {}", listen_addr, e))?;
    tracing::info!(
        "Local TCP forward: {} → [ygg] {}",
        listen_addr,
        remote
    );

    loop {
        let (local_conn, peer) = listener
            .accept()
            .await
            .map_err(|e| format!("local-tcp accept: {}", e))?;
        tracing::debug!("local-tcp new conn from {}", peer);

        let ns = netstack.clone();
        tokio::spawn(async move {
            match ns.dial_tcp(remote).await {
                Ok(ygg_conn) => {
                    if let Err(e) = proxy_tcp(local_conn, ygg_conn).await {
                        tracing::debug!("local-tcp proxy error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("local-tcp dial {} failed: {}", remote, e);
                }
            }
        });
    }
}

async fn proxy_tcp(
    mut a: TcpStream,
    mut b: NetTcpStream,
) -> io::Result<()> {
    let (mut ar, mut aw) = a.split();
    let (mut br, mut bw) = io::split(&mut b);
    tokio::select! {
        r = io::copy(&mut ar, &mut bw) => { r?; }
        r = io::copy(&mut br, &mut aw) => { r?; }
    }
    Ok(())
}

// ── Local UDP: local port → Yggdrasil remote ──────────────────────────────────

/// Listen on `listen_addr` (local/UDP) and forward datagrams to `remote`
/// on the Yggdrasil netstack (like `ssh -L` but UDP).
/// Each unique local sender gets its own smoltcp UDP socket (session).
/// Responses from the Yggdrasil remote are sent back to the originating client.
pub async fn local_udp_v2(
    listen_addr: SocketAddr,
    remote: SocketAddr,
    netstack: Arc<Netstack>,
) -> Result<(), String> {
    let local_sock = UdpSocket::bind(listen_addr)
        .await
        .map_err(|e| format!("local-udp bind {}: {}", listen_addr, e))?;
    let local_sock = Arc::new(local_sock);
    tracing::info!(
        "Local UDP forward: {} → [ygg] {}",
        listen_addr,
        remote
    );

    // Session map: client_addr → send_tx
    let sessions: Arc<Mutex<HashMap<SocketAddr, crossbeam_channel::Sender<(Bytes, SocketAddr)>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let mut buf = vec![0u8; 65536];
    loop {
        let (n, client_addr) = local_sock
            .recv_from(&mut buf)
            .await
            .map_err(|e| format!("local-udp recv: {}", e))?;
        let data = Bytes::copy_from_slice(&buf[..n]);

        let mut map = sessions.lock().await;

        if let Some(send_tx) = map.get(&client_addr) {
            // Existing session – forward the datagram
            let _ = send_tx.send((data, remote));
        } else {
            // New session
            let local_port = alloc_udp_port_v2(&map);
            match netstack.dial_udp(local_port).await {
                Ok(ygg_sock) => {
                    let send_tx = ygg_sock.send_tx.clone();
                    map.insert(client_addr, send_tx.clone());
                    let _ = send_tx.send((data, remote));
                    drop(map);

                    // Reverse proxy: Ygg → local client
                    let local_sock2 = local_sock.clone();
                    let sessions2 = sessions.clone();
                    tokio::spawn(async move {
                        let mut ygg = ygg_sock;
                        loop {
                            match tokio::time::timeout(Duration::from_secs(120), ygg.recv()).await {
                                Ok(Some((pkt, _from))) => {
                                    let _ = local_sock2.send_to(&pkt, client_addr).await;
                                }
                                Ok(None) | Err(_) => {
                                    sessions2.lock().await.remove(&client_addr);
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("local-udp dial {} failed: {}", remote, e);
                }
            }
        }
    }
}

// ── Remote TCP: Yggdrasil port → local address ────────────────────────────────

/// Listen on `ygg_port` in the Yggdrasil netstack and forward each connection
/// to `local_target` (like `ssh -R` TCP).
pub async fn remote_tcp(
    ygg_port: u16,
    local_target: SocketAddr,
    netstack: Arc<Netstack>,
) -> Result<(), String> {
    let mut listener = netstack
        .listen_tcp(ygg_port)
        .await
        .map_err(|e| format!("remote-tcp listen {}: {}", ygg_port, e))?;
    tracing::info!(
        "Remote TCP forward: [ygg] port {} → {}",
        ygg_port,
        local_target
    );

    loop {
        let ygg_conn = match listener.accept().await {
            Some(c) => c,
            None => break,
        };

        tokio::spawn(async move {
            match TcpStream::connect(local_target).await {
                Ok(local_conn) => {
                    if let Err(e) = proxy_tcp(local_conn, ygg_conn).await {
                        tracing::debug!("remote-tcp proxy error: {}", e);
                    }
                }
                Err(e) => {
                    tracing::warn!("remote-tcp connect {} failed: {}", local_target, e);
                }
            }
        });
    }
    Ok(())
}

// ── Remote UDP: Yggdrasil port → local address ────────────────────────────────

/// Listen on `ygg_port` in the Yggdrasil netstack and forward datagrams to
/// `local_target` (like `ssh -R` UDP).
pub async fn remote_udp(
    ygg_port: u16,
    local_target: SocketAddr,
    netstack: Arc<Netstack>,
) -> Result<(), String> {
    let ygg_sock = netstack
        .listen_udp(ygg_port)
        .await
        .map_err(|e| format!("remote-udp listen {}: {}", ygg_port, e))?;
    tracing::info!(
        "Remote UDP forward: [ygg] port {} → {}",
        ygg_port,
        local_target
    );

    // Sessions: Ygg sender → local UDP socket
    let sessions: Arc<Mutex<HashMap<SocketAddr, Arc<UdpSocket>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let ygg_send = ygg_sock.send_tx.clone();
    let mut ygg_recv = ygg_sock;

    loop {
        let (data, from_ygg) = match ygg_recv.recv().await {
            Some(x) => x,
            None => break,
        };

        let mut map = sessions.lock().await;

        if let Some(local_sock) = map.get(&from_ygg) {
            let _ = local_sock.send_to(&data, local_target).await;
        } else {
            // New session
            match UdpSocket::bind("0.0.0.0:0").await {
                Ok(sock) => {
                    let sock = Arc::new(sock);
                    let _ = sock.send_to(&data, local_target).await;
                    map.insert(from_ygg, sock.clone());
                    drop(map);

                    // Reverse: local → Ygg sender
                    let ygg_send2 = ygg_send.clone();
                    let sessions2 = sessions.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65536];
                        loop {
                            match tokio::time::timeout(
                                Duration::from_secs(120),
                                sock.recv_from(&mut buf),
                            )
                            .await
                            {
                                Ok(Ok((n, _))) => {
                                    let pkt = Bytes::copy_from_slice(&buf[..n]);
                                    let _ = ygg_send2.send((pkt, from_ygg));
                                }
                                _ => {
                                    sessions2.lock().await.remove(&from_ygg);
                                    break;
                                }
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("remote-udp local socket bind failed: {}", e);
                }
            }
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn alloc_udp_port_v2(
    map: &HashMap<SocketAddr, crossbeam_channel::Sender<(Bytes, SocketAddr)>>,
) -> u16 {
    // Flat allocation; fine for small numbers of sessions.
    49152 + map.len() as u16
}
