/// TCP port forwarding.
///
/// * local-tcp  — accept on a local OS port, connect via netstack to a
///   remote Yggdrasil address.
/// * remote-tcp — accept on a smoltcp TCP socket bound to our Yggdrasil
///   address, connect via OS TCP to a local address.
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;

use crate::mapping::TcpMapping;
use crate::netstack::YggNetstack;

/// Start a local-tcp forwarder.
///
/// Listens on `mapping.listen` (OS) and forwards each connection to
/// `mapping.target` (Yggdrasil via netstack).
/// The task exits cleanly when `stop` receives a value or the sender is dropped.
pub fn spawn_local_tcp(netstack: Arc<YggNetstack>, mapping: TcpMapping, stop_tx: broadcast::Sender<()>) {
    tokio::spawn(async move {
        let listener = match TcpListener::bind(mapping.listen).await {
            Ok(l) => {
                tracing::info!(
                    "local-tcp: {} → {}",
                    mapping.listen,
                    mapping.target
                );
                l
            }
            Err(e) => {
                tracing::error!("local-tcp bind {}: {}", mapping.listen, e);
                return;
            }
        };
        let mut stop = stop_tx.subscribe();
        loop {
            tokio::select! {
                _ = stop.recv() => {
                    tracing::info!("local-tcp: stopped {} → {}", mapping.listen, mapping.target);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok((client, _peer)) => {
                            let ns = netstack.clone();
                            let target = mapping.target;
                            let stop_conn = stop_tx.subscribe();
                            tokio::spawn(async move {
                                if let Err(e) = forward_local_tcp(client, ns, target, stop_conn).await {
                                    tracing::debug!("local-tcp fwd: {}", e);
                                }
                            });
                        }
                        Err(e) => tracing::warn!("local-tcp accept: {}", e),
                    }
                }
            }
        }
    });
}

async fn forward_local_tcp(
    client: TcpStream,
    netstack: Arc<YggNetstack>,
    target: SocketAddr,
    mut stop: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let ygg = netstack.dial_tcp(target).await?;
    let (mut cr, mut cw) = client.into_split();
    let (mut yr, mut yw) = tokio::io::split(ygg);
    tokio::select! {
        _ = stop.recv() => {}
        _ = tokio::io::copy(&mut cr, &mut yw) => {}
        _ = tokio::io::copy(&mut yr, &mut cw) => {}
    }
    Ok(())
}

/// Start a remote-tcp forwarder.
///
/// Listens on our Yggdrasil address/port (netstack) and forwards each
/// incoming connection to `mapping.target` (OS TCP).
/// The task exits cleanly when `stop` receives a value or the sender is dropped.
pub fn spawn_remote_tcp(netstack: Arc<YggNetstack>, mapping: TcpMapping, stop_tx: broadcast::Sender<()>) {
    let port = mapping.listen.port();
    let target = mapping.target;
    let ns = netstack.clone();
    tokio::spawn(async move {
        let listener = match ns.listen_tcp(port) {
            Ok(l) => {
                tracing::info!("remote-tcp: ygg:{} → {}", port, target);
                l
            }
            Err(e) => {
                tracing::error!("remote-tcp listen ygg:{}: {}", port, e);
                return;
            }
        };
        let mut stop = stop_tx.subscribe();
        loop {
            tokio::select! {
                _ = stop.recv() => {
                    tracing::info!("remote-tcp: stopped ygg:{} → {}", port, target);
                    break;
                }
                result = listener.accept() => {
                    match result {
                        Ok(ygg_stream) => {
                            let stop_conn = stop_tx.subscribe();
                            tokio::spawn(async move {
                                if let Err(e) = forward_remote_tcp(ygg_stream, target, stop_conn).await {
                                    tracing::debug!("remote-tcp fwd: {}", e);
                                }
                            });
                        }
                        Err(e) => tracing::warn!("remote-tcp accept: {}", e),
                    }
                }
            }
        }
    });
}

async fn forward_remote_tcp(
    ygg_stream: crate::netstack::TcpStream,
    target: SocketAddr,
    mut stop: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let local = TcpStream::connect(target).await?;
    let (mut lr, mut lw) = local.into_split();
    let (mut yr, mut yw) = tokio::io::split(ygg_stream);
    tokio::select! {
        _ = stop.recv() => {}
        _ = tokio::io::copy(&mut yr, &mut lw) => {}
        _ = tokio::io::copy(&mut lr, &mut yw) => {}
    }
    Ok(())
}
