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
use tokio::task::JoinHandle;

use crate::mapping::UdpMapping;
use crate::netstack::YggNetstack;

/// Session timeout: evict sessions idle for longer than this.
const SESSION_TTL_SECS: u64 = 30;
/// Timer-based eviction interval.
const EVICT_TIMER_SECS: u64 = 10;

struct UdpSession<S> {
    socket: Arc<S>,
    last_active: Instant,
    listener_handle: Option<JoinHandle<()>>,
}

/// Spawn a periodic eviction task for a sessions map.
fn spawn_eviction_timer<S: Send + Sync + 'static>(
    sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession<S>>>>,
    label: &'static str,
) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(EVICT_TIMER_SECS));
        loop {
            interval.tick().await;
            let mut guard = sessions.lock().await;
            let now = Instant::now();

            // Collect stale session keys
            let stale: Vec<SocketAddr> = guard
                .iter()
                .filter(|(_, s)| now.duration_since(s.last_active).as_secs() >= SESSION_TTL_SECS)
                .map(|(k, _)| *k)
                .collect();

            let evicted = stale.len();
            let mut aborted = 0;
            for addr in stale {
                if let Some(session) = guard.remove(&addr) {
                    if let Some(handle) = session.listener_handle {
                        handle.abort();
                        aborted += 1;
                    }
                    // session.socket Arc drops here -> if refcount=0 -> smoltcp remove
                }
            }
            let remaining = guard.len();
            drop(guard);

            if evicted > 0 {
                tracing::info!(
                    "{}: evicted {} sessions (aborted {} listeners), {} remaining",
                    label, evicted, aborted, remaining
                );
            }
        }
    });
}

/// Start a local-udp forwarder.
pub fn spawn_local_udp(netstack: Arc<YggNetstack>, mapping: UdpMapping) {
    tokio::spawn(async move {
        let local_sock = match OsUdpSocket::bind(mapping.listen).await {
            Ok(s) => {
                tracing::info!(
                    "local-udp: {} -> {}",
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

        // Timer-based eviction with listener abort
        spawn_eviction_timer(sessions.clone(), "local-udp");

        let mut buf = vec![0u8; 65535];
        loop {
            match local_sock.recv_from(&mut buf).await {
                Ok((n, from)) => {
                    let data = buf[..n].to_vec();
                    let target = mapping.target;
                    let ns = netstack.clone();
                    let sessions2 = sessions.clone();
                    let local_sock2 = local_sock.clone();

                    tokio::spawn(async move {
                        let (udp_sock, need_listener) = {
                            let mut guard = sessions2.lock().await;
                            let session = guard.entry(from).or_insert_with(|| {
                                UdpSession {
                                    socket: Arc::new(ns.open_udp().unwrap()),
                                    last_active: Instant::now(),
                                    listener_handle: None,
                                }
                            });
                            session.last_active = Instant::now();
                            let need = session.listener_handle
                                .as_ref()
                                .map_or(true, |h| h.is_finished());
                            (session.socket.clone(), need)
                        };

                        if let Err(e) = udp_sock.send_to(&data, target).await {
                            tracing::debug!("local-udp send: {}", e);
                            return;
                        }

                        if need_listener {
                            let udp_sock2 = udp_sock.clone();
                            let from2 = from;
                            let handle = tokio::spawn(async move {
                                let mut rbuf = vec![0u8; 65535];
                                while let Ok((rn, _src)) = udp_sock2.recv_from(&mut rbuf).await {
                                    let _ = local_sock2.send_to(&rbuf[..rn], from2).await;
                                }
                            });
                            // Store handle for abort on eviction
                            let mut guard = sessions2.lock().await;
                            if let Some(session) = guard.get_mut(&from) {
                                session.listener_handle = Some(handle);
                            }
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
                tracing::info!("remote-udp: ygg:{} -> {}", port, target);
                Arc::new(s)
            }
            Err(e) => {
                tracing::error!("remote-udp bind ygg:{}: {}", port, e);
                return;
            }
        };

        let sessions: Arc<Mutex<HashMap<SocketAddr, UdpSession<OsUdpSocket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        // Timer-based eviction with listener abort
        spawn_eviction_timer(sessions.clone(), "remote-udp");

        let mut buf = vec![0u8; 65535];
        loop {
            match ygg_sock.recv_from(&mut buf).await {
                Ok((n, from_ygg)) => {
                    let data = buf[..n].to_vec();
                    let ygg_sock2 = ygg_sock.clone();
                    let sessions2 = sessions.clone();

                    tokio::spawn(async move {
                        let (local_sock, need_listener) = {
                            let mut guard = sessions2.lock().await;
                            if !guard.contains_key(&from_ygg) {
                                match OsUdpSocket::bind("0.0.0.0:0").await {
                                    Ok(s) => {
                                        guard.insert(from_ygg, UdpSession {
                                            socket: Arc::new(s),
                                            last_active: Instant::now(),
                                            listener_handle: None,
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
                            let need = session.listener_handle
                                .as_ref()
                                .map_or(true, |h| h.is_finished());
                            (session.socket.clone(), need)
                        };

                        if let Err(e) = local_sock.send_to(&data, target).await {
                            tracing::debug!("remote-udp local send: {}", e);
                        }

                        if need_listener {
                            let handle = tokio::spawn(async move {
                                let mut rbuf = vec![0u8; 65535];
                                while let Ok((rn, _src)) = local_sock.recv_from(&mut rbuf).await {
                                    let _ = ygg_sock2.send_to(&rbuf[..rn], from_ygg).await;
                                }
                            });
                            let mut guard = sessions2.lock().await;
                            if let Some(session) = guard.get_mut(&from_ygg) {
                                session.listener_handle = Some(handle);
                            }
                        }
                    });
                }
                Err(e) => tracing::warn!("remote-udp recv: {}", e),
            }
        }
    });
}
