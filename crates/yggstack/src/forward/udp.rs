/// UDP port forwarding.
///
/// * local-udp  — bind a local OS UDP socket, forward datagrams to a
///   remote Yggdrasil address via the netstack UDP socket.
/// * remote-udp — bind a netstack UDP socket on our Yggdrasil address,
///   forward datagrams to/from a local OS UDP address.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::UdpSocket as OsUdpSocket;
use tokio::sync::Mutex;

use crate::mapping::UdpMapping;
use crate::netstack::YggNetstack;

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

        // Map client OS address → dedicated netstack UDP socket.
        let sessions: Arc<Mutex<HashMap<SocketAddr, Arc<crate::netstack::UdpSocket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

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
                        let udp_sock = {
                            let mut guard = sessions2.lock().await;
                            guard.entry(from).or_insert_with(|| {
                                Arc::new(ns.open_udp().unwrap())
                            }).clone()
                        };

                        if let Err(e) = udp_sock.send_to(&data, target).await {
                            tracing::debug!("local-udp send: {}", e);
                            return;
                        }

                        // Spawn return-path listener if not already running.
                        // (Simplified: always spawn; duplicates are benign.)
                        let udp_sock2 = udp_sock.clone();
                        let from2 = from;
                        tokio::spawn(async move {
                            let mut rbuf = vec![0u8; 65535];
                            while let Ok((rn, _src)) = udp_sock2.recv_from(&mut rbuf).await {
                                let _ = local_sock2.send_to(&rbuf[..rn], from2).await;
                            }
                        });
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

        // Map remote Yggdrasil address → local OS UDP socket.
        let sessions: Arc<Mutex<HashMap<SocketAddr, Arc<OsUdpSocket>>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let mut buf = vec![0u8; 65535];
        loop {
            match ygg_sock.recv_from(&mut buf).await {
                Ok((n, from_ygg)) => {
                    let data = buf[..n].to_vec();
                    let ygg_sock2 = ygg_sock.clone();
                    let sessions2 = sessions.clone();

                    tokio::spawn(async move {
                        // Get or create local OS socket for this Yggdrasil peer.
                        let local_sock = {
                            let mut guard = sessions2.lock().await;
                            if let Some(s) = guard.get(&from_ygg) {
                                s.clone()
                            } else {
                                match OsUdpSocket::bind("0.0.0.0:0").await {
                                    Ok(s) => {
                                        let s = Arc::new(s);
                                        guard.insert(from_ygg, s.clone());
                                        s
                                    }
                                    Err(e) => {
                                        tracing::warn!("remote-udp local bind: {}", e);
                                        return;
                                    }
                                }
                            }
                        };

                        if let Err(e) = local_sock.send_to(&data, target).await {
                            tracing::debug!("remote-udp local send: {}", e);
                        }

                        // Spawn return-path task.
                        tokio::spawn(async move {
                            let mut rbuf = vec![0u8; 65535];
                            while let Ok((rn, _src)) = local_sock.recv_from(&mut rbuf).await {
                                let _ = ygg_sock2.send_to(&rbuf[..rn], from_ygg).await;
                            }
                        });
                    });
                }
                Err(e) => tracing::warn!("remote-udp recv: {}", e),
            }
        }
    });
}
