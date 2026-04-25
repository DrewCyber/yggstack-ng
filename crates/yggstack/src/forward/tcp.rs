/// TCP port forwarding.
///
/// * local-tcp  — accept on a local OS port, connect via netstack to a
///   remote Yggdrasil address.
/// * remote-tcp — accept on a smoltcp TCP socket bound to our Yggdrasil
///   address, connect via OS TCP to a local address.
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::{TcpListener, TcpStream};

use crate::mapping::TcpMapping;
use crate::netstack::YggNetstack;

/// Start a local-tcp forwarder.
///
/// Listens on `mapping.listen` (OS) and forwards each connection to
/// `mapping.target` (Yggdrasil via netstack).
pub fn spawn_local_tcp(netstack: Arc<YggNetstack>, mapping: TcpMapping) {
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
        loop {
            match listener.accept().await {
                Ok((client, _peer)) => {
                    let ns = netstack.clone();
                    let target = mapping.target;
                    tokio::spawn(async move {
                        if let Err(e) = forward_local_tcp(client, ns, target).await {
                            tracing::debug!("local-tcp fwd: {}", e);
                        }
                    });
                }
                Err(e) => tracing::warn!("local-tcp accept: {}", e),
            }
        }
    });
}

async fn forward_local_tcp(
    client: TcpStream,
    netstack: Arc<YggNetstack>,
    target: SocketAddr,
) -> std::io::Result<()> {
    let ygg = netstack.dial_tcp(target).await?;
    let (mut cr, mut cw) = client.into_split();
    let (mut yr, mut yw) = tokio::io::split(ygg);
    tokio::select! {
        _ = tokio::io::copy(&mut cr, &mut yw) => {}
        _ = tokio::io::copy(&mut yr, &mut cw) => {}
    }
    Ok(())
}

/// Start a remote-tcp forwarder.
///
/// Listens on our Yggdrasil address/port (netstack) and forwards each
/// incoming connection to `mapping.target` (OS TCP).
pub fn spawn_remote_tcp(netstack: Arc<YggNetstack>, mapping: TcpMapping) {
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
        loop {
            match listener.accept().await {
                Ok(ygg_stream) => {
                    tokio::spawn(async move {
                        if let Err(e) = forward_remote_tcp(ygg_stream, target).await {
                            tracing::debug!("remote-tcp fwd: {}", e);
                        }
                    });
                }
                Err(e) => tracing::warn!("remote-tcp accept: {}", e),
            }
        }
    });
}

async fn forward_remote_tcp(
    ygg_stream: crate::netstack::TcpStream,
    target: SocketAddr,
) -> std::io::Result<()> {
    let local = TcpStream::connect(target).await?;
    let (mut lr, mut lw) = local.into_split();
    let (mut yr, mut yw) = tokio::io::split(ygg_stream);
    tokio::select! {
        _ = tokio::io::copy(&mut yr, &mut lw) => {}
        _ = tokio::io::copy(&mut lr, &mut yw) => {}
    }
    Ok(())
}
