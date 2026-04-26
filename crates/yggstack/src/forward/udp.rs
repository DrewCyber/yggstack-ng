/// UDP port forwarding.
///
/// * local-udp  — bind a local OS UDP socket, forward datagrams to a
///   remote Yggdrasil address via the netstack UDP socket.
/// * remote-udp — bind a netstack UDP socket on our Yggdrasil address,
///   forward datagrams to/from a local OS UDP address.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use tokio::net::UdpSocket as OsUdpSocket;
use tokio::sync::Mutex;

use crate::mapping::UdpMapping;
use crate::netstack::YggNetstack;

/// Session timeout: evict sessions idle for longer than this.
const SESSION_TTL_SECS: u64 = 30;
/// How often to run eviction (every N packets).
const EVICT_INTERVAL: u64 = 20;

struct UdpSession<S> {
    socket: Arc<S>,
    last_active: Instant,
    has_listener: bool,
}

/// Start a local-udp forwarder.
pub fn spawn_local_udp(netstack: Arc<YggNetstack>, mapping: UdpMapping) {
    tokio::spawn(async move {
        let local_sock = match OsUdpSocket::bind(mapping.listen).await {
            Ok(s) => {
                tracing::info!(
                    "local-udp: {} → {}",
                    mapping.listen,
                    mapping.target
                );
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!("local-udp bind {}: {}", mapping.listen, e);
                return;
            }
        };

        let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession<crate::netstack::UdpSocket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mut buf = vec![0u8; 65535];
        let mut pkt_count: u64 = 0;
        loop {
            match local_sock.recv_from(&mut buf).await {
                Ok((n, from)) => {
                    let data = buf[..n].to_vec();
                    let target = mapping.target;
                    let ns = netstack.clone();
                    let sessions2 = sessions.clone();
                    let local_sock2 = local_sock.clone();

                    // Periodic eviction
                    pkt_count += 1;
                    if pkt_count % EVICT_INTERVAL == 0 {
                        let mut guard = sessions.lock().await;
                        let now = Instant::now();
                        let before = guard.len();
                        guard.retain(|_, s| now.duration_since(s.last_active).as_secs() < SESSION_TTL_SECS);
                        let evicted = before - guard.len();
                        if evicted > 0 {
                            tracing::debug!("local-udp: evicted {} stale sessions, {} remaining", evicted, guard.len());
                        }
                    }

                    tokio::spawn(async move {
                        let (udp_sock, need_listener) = {
                            let mut guard = sessions2.lock().await;
                            let session = guard.entry(from).or_insert_with(|| {
                                UdpSession {
                                    socket: Arc::new(ns.open_udp().unwrap()),
                                    last_active: Instant::now(),
                                    has_listener: false,
                                }
                            });
                            session.last_active = Instant::now();
                            let need = !session.has_listener;
                            if need {
                                session.has_listener = true;
                            }
                            (session.socket.clone(), need)
                        };

                        if let Err(e) = udp_sock.send_to(&data, target).await {
                            tracing::debug!("local-udp send: {}", e);
                            return;
                        }

                        if need_listener {
                            let udp_sock2 = udp_sock.clone();
                            let from2 = from;
                            tokio::spawn(async move {
                                let mut rbuf = vec![0u8; 65535];
                                while let Ok((rn, _src)) = udp_sock2.recv_from(&mut rbuf).await {
                                    let _ = local_sock2.send_to(&rbuf[..rn], from2).await;
                                }
                            });
                        }
                    });
                }
                Err(e) => tracing::warn!("local-udp recv: {}", e),
            }
        }
    });
}

/// Start a remote-udp forwarder.
pub fn spawn_remote_udp(netstack: Arc<YggNetstack>, mapping: UdpMapping) {
    let port = mapping.listen.port();
    let target = mapping.target;
    let ns = netstack.clone();

    tokio::spawn(async move {
        let ygg_sock = match ns.bind_udp(port) {
            Ok(s) => {
                tracing::info!("remote-udp: ygg:{} → {}", port, target);
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!("remote-udp bind ygg:{}: {}", port, e);
                return;
            }
        };

        let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession<OsUdpSocket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mut buf = vec![0u8; 65535];
        let mut pkt_count: u64 = 0;
        loop {
            match ygg_sock.recv_from(&mut buf).await {
                Ok((n, from_ygg)) => {
                    let data = buf[..n].to_vec();
                    let ygg_sock2 = ygg_sock.clone();
                    let sessions2 = sessions.clone();

                    // Periodic eviction
                    pkt_count += 1;
                    if pkt_count % EVICT_INTERVAL == 0 {
                        let mut guard = sessions.lock().await;
                        let now = Instant::now();
                        let before = guard.len();
                        guard.retain(|_, s| now.duration_since(s.last_active).as_secs() < SESSION_TTL_SECS);
                        let evicted = before - guard.len();
                        if evicted > 0 {
                            tracing::debug!("remote-udp: evicted {} stale sessions, {} remaining", evicted, guard.len());
                        }
                    }

                    tokio::spawn(async move {
                        let (local_sock, need_listener) = {
                            let mut guard = sessions2.lock().await;
                            if !guard.contains_key(&from_ygg) {
                                match OsUdpSocket::bind("0.0.0.0:0").await {
                                    Ok(s) => {
                                        guard.insert(from_ygg, UdpSession {
                                            socket: Arc::new(s),
                                            last_active: Instant::now(),
                                            has_listener: false,
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!("remote-udp local bind: {}", e);
                                        return;
                                    }
                                }
                            }
                            let session = guard.get_mut(&from_ygg).unwrap();
                            session.last_active = Instant::now();
                            let need = !session.has_listener;
                            if need {
                                session.has_listener = true;
                            }
                            (session.socket.clone(), need)
                        };

                        if let Err(e) = local_sock.send_to(&data, target).await {
                            tracing::debug!("remote-udp local send: {}", e);
                        }

                        if need_listener {
                            tokio::spawn(async move {
                                let mut rbuf = vec![0u8; 65535];
                                while let Ok((rn, _src)) = local_sock.recv_from(&mut rbuf).await {
                                    let _ = ygg_sock2.send_to(&rbuf[..rn], from_ygg).await;
                                }
                            });
                        }
                    });
                }
                Err(e) => tracing::warn!("remote-udp recv: {}", e),
            }
        }
    });
}
