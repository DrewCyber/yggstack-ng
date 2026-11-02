//! UniFFI mobile bindings for yggstack.
use std::net::Ipv6Addr;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use yggdrasil::core::Core;
use yggdrasil::ipv6rwc::ReadWriteCloser;

use yggstack::config;
use yggstack::forward::tcp::{local_tcp_key, remote_tcp_key, spawn_local_tcp, spawn_remote_tcp};
use yggstack::forward::udp::{local_udp_key, remote_udp_key, spawn_local_udp, spawn_remote_udp};
use yggstack::http_proxy::HttpProxyServer;
use yggstack::mapping::{TcpMapping, UdpMapping};
use yggstack::netstack::YggNetstack;
use yggstack::ping::{self, PingDoneReason, PingEvent};
use yggstack::resolver::NameResolver;
use yggstack::socks::Socks5Server;
use yggstack::stats::ListenerStatsRegistry;

// ── Tracing init ──────────────────────────────────────────────────────────────

/// App-side log sink (set from set_log_callback; absent = nobody listening).
static LOG_SINK: Mutex<Option<Box<dyn LogCallback>>> = Mutex::new(None);

/// Verbosity for the app callback layer: 0=error 1=warn 2=info 3=debug 4=trace.
static LOG_LEVEL: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(2);

/// Filter that gates events for the app callback by the global LOG_LEVEL.
#[derive(Clone, Copy)]
struct CallbackFilter;

impl<S: tracing::Subscriber> tracing_subscriber::layer::Filter<S> for CallbackFilter {
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        _: &tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let lvl = match *meta.level() {
            tracing::Level::ERROR => 0u8,
            tracing::Level::WARN => 1,
            tracing::Level::INFO => 2,
            tracing::Level::DEBUG => 3,
            tracing::Level::TRACE => 4,
        };
        lvl <= LOG_LEVEL.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Write target that forwards formatted events into the app's LogCallback.
struct CallbackWriter;

impl std::io::Write for CallbackWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let line = String::from_utf8_lossy(buf);
        if let Some(cb) = LOG_SINK.lock().unwrap().as_ref() {
            cb.on_log(line.trim_end().to_string());
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Copy, Default)]
struct MakeCallbackWriter;

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeCallbackWriter {
    type Writer = CallbackWriter;
    fn make_writer(&'a self) -> Self::Writer {
        CallbackWriter
    }
}

fn init_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::EnvFilter;

        use tracing_subscriber::Layer as _;

        let logcat_filter = EnvFilter::new("yggstack=info,yggdrasil=info,ironwood=warn");

        // App callback layer: level adjustable at runtime via set_log_level.
        let callback_layer = tracing_subscriber::fmt::layer()
            .with_writer(MakeCallbackWriter)
            .with_ansi(false)
            .without_time();

        #[cfg(target_os = "android")]
        let logcat_layer = tracing_android::layer("yggstack").unwrap();
        #[cfg(not(target_os = "android"))]
        let logcat_layer = tracing_subscriber::fmt::layer();

        tracing_subscriber::registry()
            .with(logcat_layer.with_filter(logcat_filter))
            .with(callback_layer.with_filter(CallbackFilter))
            .init();
    });
}

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum YggstackError {
    #[error("Config: {0}")]
    Config(String),
    #[error("Runtime: {0}")]
    Runtime(String),
    #[error("Io: {0}")]
    Io(String),
    #[error("AlreadyRunning")]
    AlreadyRunning(String),
    #[error("NotRunning")]
    NotRunning(String),
}

// ── LogCallback (UniFFI callback interface) ───────────────────────────────────

pub trait LogCallback: Send + Sync {
    fn on_log(&self, message: String);
}

// ── PingCallback (UniFFI callback interface) ─────────────────────────────────

pub trait PingCallback: Send + Sync {
    fn on_result(&self, result: String);
}

// ── PeerStatusCallback (UniFFI callback interface) ───────────────────────────

pub trait PeerStatusCallback: Send + Sync {
    fn on_peers_changed(&self, peers_json: String);
}

/// Debounce window coalescing bursts of peer events (e.g. a network flap
/// dropping several peers at once) into a single snapshot delivery.
const PEER_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

/// Render the core's peer list in the shared JSON schema. Used by both
/// get_peers_json() and the peer-events forwarder, so the snapshot a
/// callback delivers is byte-identical to what polling would return.
async fn peers_snapshot_json(core: &Arc<Core>) -> String {
    let peers = core.get_peers().await;
    let items: Vec<String> = peers
        .iter()
        .map(|p| {
            let uri_json = serde_json::to_string(&p.uri).unwrap_or_else(|_| "\"\"".to_string());
            let port = p
                .uri
                .split('?')
                .next()
                .and_then(|u| u.rsplit(':').next())
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(0);
            // The Yggdrasil address derived from the peer's public key;
            // empty for configured-but-never-connected peers (zero key).
            let addr = if p.key.iter().any(|b| *b != 0) {
                config::addr_for_key(&p.key).to_string()
            } else {
                String::new()
            };
            format!(
                r#"{{"URI":{uri},"Up":{up},"Inbound":{inbound},"Address":{addr},"Port":{port},"Priority":{prio},"Cost":{cost},"RXBytes":{rx},"TXBytes":{tx},"Uptime":{uptime:.0},"Latency":{latency:.0}}}"#,
                uri = uri_json,
                up = p.up,
                inbound = p.inbound,
                addr = serde_json::to_string(&addr).unwrap_or_else(|_| "\"\"".to_string()),
                port = port,
                prio = p.priority,
                cost = p.cost,
                rx = p.rx_bytes,
                tx = p.tx_bytes,
                uptime = p.uptime_secs * 1_000_000_000.0,
                latency = p.latency_ms * 1_000_000.0,
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// Render one [PingEvent] as the JSON protocol shared with the Go engine.
fn ping_event_json(ev: PingEvent) -> String {
    match ev {
        PingEvent::Probe { seq, rtt, error } => match rtt {
            Some(rtt) => format!(
                r#"{{"Type":"probe","Seq":{seq},"Success":true,"RttMs":{:.2}}}"#,
                rtt.as_secs_f64() * 1000.0
            ),
            None => format!(
                r#"{{"Type":"probe","Seq":{seq},"Success":false,"Error":"{}"}}"#,
                error.as_deref().unwrap_or("timeout")
            ),
        },
        PingEvent::Done { reason } => {
            let reason = match reason {
                PingDoneReason::Completed => "completed",
                PingDoneReason::Stopped => "stopped",
                PingDoneReason::Failed => "error",
            };
            format!(r#"{{"Type":"done","Reason":"{reason}"}}"#)
        }
    }
}

/// A running ping session: firing `stop_tx` cancels the probe loop and
/// `handle` lets stop_ping() join the task so no callback event can race a
/// newly started session.
struct PingSession {
    stop_tx: tokio::sync::watch::Sender<bool>,
    handle: tokio::task::JoinHandle<()>,
}

/// Grace period for a ping task to exit after stop before aborting it.
const PING_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Future that resolves once the watch channel reports `true` (or its
/// sender is dropped, which also ends the session).
fn watch_stop(mut rx: tokio::sync::watch::Receiver<bool>) -> impl std::future::Future<Output = ()> {
    async move {
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Parse "[200:...]:port" and verify it is a Yggdrasil address (200::/7).
fn parse_ygg_addr_port(s: &str) -> Option<std::net::SocketAddr> {
    let addr: std::net::SocketAddr = s.trim().parse().ok()?;
    match addr {
        std::net::SocketAddr::V6(a) if ping::is_ygg_address(&a.ip()) => Some(addr),
        _ => None,
    }
}

/// Pseudo-random ephemeral port for probe sockets (same range open_udp
/// uses); collisions retry in the caller.
fn probe_ephemeral_port() -> u16 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as u16) % 16384 + 49152
}

/// The probe datagram for a UDP port. Port 53 is DNS by assignment: a
/// minimal well-formed root NS query — any DNS reply (NOERROR, REFUSED,
/// FORMERR…) proves a live service, where a zero-filled datagram is
/// silently dropped by most servers. Other ports get the generic zero
/// probe, which reply-happy protocols still answer.
fn udp_probe_payload(port: u16) -> Vec<u8> {
    if port == 53 {
        vec![
            0x12, 0x34, // ID
            0x01, 0x00, // flags: RD
            0x00, 0x01, // QDCOUNT: 1
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // counts: 0
            0x00,       // QNAME: root
            0x00, 0x02, // QTYPE: NS
            0x00, 0x01, // QCLASS: IN
        ]
    } else {
        vec![0u8; 16]
    }
}

// ── Namespace functions ───────────────────────────────────────────────────────

pub fn generate_config() -> String {
    config::generate_text()
}

pub fn get_version() -> String {
    format!("yggstack {}", env!("CARGO_PKG_VERSION"))
}

/// Measure RTT to a QUIC peer by completing the QUIC/TLS handshake
/// (TLS 1.3, no ALPN, certificates accepted as-is). Returns RTT in
/// milliseconds, or -1 on failure/timeout (5s budget incl. DNS).
pub fn check_quic_peer(uri: String) -> i64 {
    crate::quic_check::check_quic_rtt(&uri)
}

// ── Running node state ────────────────────────────────────────────────────────

struct NodeState {
    core: Arc<Core>,
    _rwc: Arc<ReadWriteCloser>,
    netstack: Arc<YggNetstack>,
    stop_tx: tokio::sync::broadcast::Sender<()>,
    stats: Arc<ListenerStatsRegistry>,
    socks_handle: Option<tokio::task::JoinHandle<()>>,
    http_handle: Option<tokio::task::JoinHandle<()>>,
    /// Peer-events forwarder task for the registered PeerStatusCallback.
    peer_events_handle: Option<tokio::task::JoinHandle<()>>,
}

/// A listener started for one mapping; firing `stop_tx` stops it and
/// `handle` lets stop() join the task so its socket is closed before
/// returning (Go's Stop() joins every worker the same way).
struct RunningListener {
    /// Keeps the channel's initial receiver alive so a stop fired before the
    /// task first polls its own subscription is not lost.
    _rx: tokio::sync::broadcast::Receiver<()>,
    stop_tx: tokio::sync::broadcast::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

/// Grace period for a listener task to exit after stop before aborting it.
const LISTENER_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// Grace period for the runtime shutdown in Drop: how long to wait for the
/// remaining tasks (core proto tasks, relay stragglers) to be dropped before
/// the runtime is torn down regardless.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

async fn join_listener(handle: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(LISTENER_STOP_GRACE, &mut *handle)
        .await
        .is_err()
    {
        handle.abort();
        let _ = handle.await;
    }
}

// ── YggstackMobile ────────────────────────────────────────────────────────────

pub struct YggstackMobile {
    rt: tokio::runtime::Runtime,
    state: Mutex<Option<NodeState>>,
    cfg: Mutex<Option<yggdrasil::config::Config>>,
    socks_addr: Mutex<Option<String>>,
    http_addr: Mutex<Option<String>>,
    nameserver: Mutex<String>,
    local_tcp: Mutex<Vec<TcpMapping>>,
    local_udp: Mutex<Vec<UdpMapping>>,
    remote_tcp: Mutex<Vec<TcpMapping>>,
    remote_udp: Mutex<Vec<UdpMapping>>,
    /// Live listeners for the current run, keyed by the same stats key the
    /// forwarders register under. Empty when the node is not running.
    listeners: Mutex<std::collections::HashMap<String, RunningListener>>,
    /// The running ping session, if any.
    ping: Mutex<Option<PingSession>>,
    /// App callback receiving peer snapshots; consumed by start() when the
    /// peer-events forwarder is spawned.
    peer_status_callback: Mutex<Option<std::sync::Arc<dyn PeerStatusCallback>>>,
}

impl Default for YggstackMobile {
    fn default() -> Self {
        Self::new()
    }
}

impl YggstackMobile {
    pub fn new() -> Self {
        init_tracing();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime");
        Self {
            rt,
            state: Mutex::new(None),
            cfg: Mutex::new(None),
            socks_addr: Mutex::new(None),
            http_addr: Mutex::new(None),
            nameserver: Mutex::new(String::new()),
            local_tcp: Mutex::new(Vec::new()),
            local_udp: Mutex::new(Vec::new()),
            remote_tcp: Mutex::new(Vec::new()),
            remote_udp: Mutex::new(Vec::new()),
            listeners: Mutex::new(std::collections::HashMap::new()),
            ping: Mutex::new(None),
            peer_status_callback: Mutex::new(None),
        }
    }

    pub fn set_log_callback(&self, callback: Box<dyn LogCallback>) {
        *LOG_SINK.lock().unwrap() = Some(callback);
    }

    pub fn set_peer_status_callback(&self, callback: Box<dyn PeerStatusCallback>) {
        *self.peer_status_callback.lock().unwrap() = Some(std::sync::Arc::from(callback));
    }

    /// Set the app-facing log verbosity ("error".."trace"). Controls which
    /// events reach the LogCallback; the logcat output keeps its own filter.
    pub fn set_log_level(&self, level: String) {
        let lvl = match level.to_ascii_lowercase().as_str() {
            "error" => 0u8,
            "warn" => 1,
            "trace" => 4,
            "debug" => 3,
            _ => 2,
        };
        LOG_LEVEL.store(lvl, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn load_config(&self, toml_config: String) -> Result<(), YggstackError> {
        let mut cfg: yggdrasil::config::Config =
            toml::from_str(&toml_config).map_err(|e| YggstackError::Config(e.to_string()))?;
        cfg.if_name = "none".to_string();
        cfg.admin_listen = "none".to_string();
        *self.cfg.lock().unwrap() = Some(cfg);
        Ok(())
    }

    pub fn generate_and_load_config(&self) -> Result<(), YggstackError> {
        self.load_config(config::generate_text())
    }

    pub fn get_config(&self) -> String {
        let guard = self.cfg.lock().unwrap();
        guard
            .as_ref()
            .and_then(|c| toml::to_string_pretty(c).ok())
            .unwrap_or_default()
    }

    pub fn get_address(&self) -> Result<String, YggstackError> {
        let guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_ref()
            .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?;
        let key = cfg.signing_key().map_err(YggstackError::Config)?;
        let pk = key.verifying_key().to_bytes();
        Ok(config::addr_for_key(&pk).to_string())
    }

    pub fn get_subnet(&self) -> Result<String, YggstackError> {
        let guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_ref()
            .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?;
        let key = cfg.signing_key().map_err(YggstackError::Config)?;
        let pk = key.verifying_key().to_bytes();
        let (ip, pfx) = config::subnet_for_key(&pk);
        Ok(format!("{}/{}", ip, pfx))
    }

    pub fn get_public_key(&self) -> Result<String, YggstackError> {
        let guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_ref()
            .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?;
        let key = cfg.signing_key().map_err(YggstackError::Config)?;
        Ok(hex::encode(key.verifying_key().to_bytes()))
    }

    pub fn start(&self) -> Result<(), YggstackError> {
        {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if guard.is_some() {
                return Err(YggstackError::AlreadyRunning("already running".to_string()));
            }
        }

        let cfg = {
            let guard = self.cfg.lock().unwrap();
            guard
                .clone()
                .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?
        };

        let signing_key = cfg.signing_key().map_err(YggstackError::Config)?;

        let socks_addr = self.socks_addr.lock().unwrap().clone();
        let http_addr = self.http_addr.lock().unwrap().clone();
        let nameserver = self.nameserver.lock().unwrap().clone();
        let local_tcp = self.local_tcp.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let local_udp = self.local_udp.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let remote_tcp = self.remote_tcp.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let remote_udp = self.remote_udp.lock().unwrap_or_else(|e| e.into_inner()).clone();

        let want_multicast = !cfg.multicast_interfaces.is_empty();
        let node = self.rt.block_on(async {
            let pk = signing_key.verifying_key().to_bytes();
            let our_addr = config::addr_for_key(&pk);
            #[cfg(feature = "ckr")]
            let tunnel_routing = cfg.tunnel_routing.clone();
            let core = Core::new(signing_key, cfg);
            core.init_links().await;
            core.start().await;
            if want_multicast {
                // Beacon/listen per multicast_interfaces; the core handles
                // interface discovery (Android can override via
                // update_network_interfaces, exposed later if needed).
                if let Err(e) = core.start_multicast().await {
                    tracing::warn!("multicast discovery: {}", e);
                }
            }
            let mtu = core.mtu();
            let rwc = ReadWriteCloser::new(
                core.clone(),
                mtu,
                #[cfg(feature = "ckr")]
                Some(&tunnel_routing),
                // Firewall stays disabled here too — see yggstack/src/main.rs.
                None,
            );
            core.set_path_notify(rwc.clone());

            let netstack = YggNetstack::new(
                rwc.clone(),
                our_addr,
                mtu,
                #[cfg(feature = "ckr")]
                Some(&tunnel_routing),
            );

            let resolver = Arc::new(NameResolver::new(netstack.clone(), &nameserver));

            let stats = ListenerStatsRegistry::new();
            let (stop_tx, _) = tokio::sync::broadcast::channel(1);

            // Peer-events forwarder: on every connect/disconnect (debounced)
            // push a fresh peer snapshot through the app callback. Events are
            // only triggers — the snapshot is always re-queried from the
            // core, so a missed or lagged event can never produce a stale
            // count. The baseline delivery right after subscribing gives the
            // consumer its initial state without polling.
            let mut peer_events_handle = None;
            {
                let peer_cb = self.peer_status_callback.lock().unwrap().clone();
                if let Some(cb) = peer_cb {
                    let core2 = core.clone();
                    let mut stop_rx = stop_tx.subscribe();
                    peer_events_handle = Some(tokio::spawn(async move {
                        let mut events = core2.subscribe_peer_events();
                        cb.on_peers_changed(peers_snapshot_json(&core2).await);
                        loop {
                            let ev = tokio::select! {
                                _ = stop_rx.recv() => break,
                                ev = events.recv() => ev,
                            };
                            match ev {
                                Ok(_) => {}
                                // Lagged: events were dropped; the snapshot
                                // below is the truth, so just continue.
                                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                                Err(_) => break,
                            }
                            let stopped = tokio::select! {
                                _ = stop_rx.recv() => true,
                                _ = tokio::time::sleep(PEER_DEBOUNCE) => false,
                            };
                            if stopped {
                                break;
                            }
                            while events.try_recv().is_ok() {}
                            cb.on_peers_changed(peers_snapshot_json(&core2).await);
                        }
                    }));
                }
            }

            let mut socks_handle = None;
            if let Some(addr) = socks_addr {
                let srv = Arc::new(Socks5Server::new(netstack.clone(), resolver.clone()));
                let a2 = addr.clone();
                let stop_clone = stop_tx.clone();
                let stats2 = stats.clone();
                socks_handle = Some(tokio::spawn(async move {
                    if let Err(e) = srv.serve_tcp(&a2, stop_clone, stats2).await {
                        tracing::error!("SOCKS5: {}", e);
                    }
                }));
            }

            let mut http_handle = None;
            if let Some(addr) = http_addr {
                let srv = Arc::new(HttpProxyServer::new(netstack.clone(), resolver.clone()));
                let a2 = addr.clone();
                let stop_clone = stop_tx.clone();
                let stats2 = stats.clone();
                http_handle = Some(tokio::spawn(async move {
                    if let Err(e) = srv.serve_tcp(&a2, stop_clone, stats2).await {
                        tracing::error!("HTTP proxy: {}", e);
                    }
                }));
            }

            let mut listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            for m in local_tcp {
                self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| {
                    spawn_local_tcp(ns, m, tx, st)
                });
            }
            for m in local_udp {
                self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| {
                    spawn_local_udp(ns, m, tx, st)
                });
            }
            for m in remote_tcp {
                self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| {
                    spawn_remote_tcp(ns, m, tx, st)
                });
            }
            for m in remote_udp {
                self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| {
                    spawn_remote_udp(ns, m, tx, st)
                });
            }

            NodeState {
                core,
                _rwc: rwc,
                netstack,
                stop_tx,
                stats,
                socks_handle,
                http_handle,
                peer_events_handle,
            }
        });

        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = Some(node);
        Ok(())
    }

    pub fn stop(&self) {
        // End the ping session first so its task and callback events stop
        // before the node (and with it the netstack) is torn down.
        self.stop_ping();
        let node = { self.state.lock().unwrap_or_else(|e| e.into_inner()).take() };
        if let Some(mut node) = node {
            // Stop the peer-events forwarder first: its task snapshots the
            // core that is torn down below, and its callback must not fire
            // against a half-stopped node.
            if let Some(handle) = node.peer_events_handle.as_mut() {
                let _ = node.stop_tx.send(());
                self.rt.block_on(join_listener(handle));
            }
            self.rt.block_on(node.core.close_multicast());
            // Close links + the ironwood core as well. Without this the old
            // instance keeps its peer connections alive under the same node
            // identity; after a restart, remote peers may keep routing
            // inbound traffic over the dead instance's links until they hit
            // their idle/keepalive timeout (observed as a 1-2 minute stall
            // of all forwarded connections after a stop/start cycle).
            let _ = self.rt.block_on(node.core.close());
            let _ = node.stop_tx.send(());
            // Join the SOCKS/HTTP servers so the proxy ports are released
            // before stop() returns (Go joins all workers in Stop()).
            if let Some(mut socks) = node.socks_handle {
                self.rt.block_on(join_listener(&mut socks));
            }
            if let Some(mut http) = node.http_handle {
                self.rt.block_on(join_listener(&mut http));
            }
            // Shut the netstack down: fail pending dials and reads, join its
            // background tasks. Without this, dial stragglers keep running on
            // the old runtime for up to CONNECT_TIMEOUT after stop().
            self.rt.block_on(node.netstack.shutdown());
        }
        // Stop and JOIN every per-mapping listener so their listening
        // sockets are closed before stop() returns — an immediate restart
        // can then rebind the same ports without racing the old tasks.
        let listeners: Vec<RunningListener> = {
            let mut guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().map(|(_, v)| v).collect()
        };
        let mut handles = Vec::new();
        for l in listeners {
            let _ = l.stop_tx.send(());
            handles.push(l.handle);
        }
        self.rt.block_on(async {
            for h in &mut handles {
                join_listener(h).await;
            }
        });
    }

    pub fn set_socks(&self, addr: String) {
        *self.socks_addr.lock().unwrap() = if addr.is_empty() { None } else { Some(addr) };
    }

    pub fn set_http(&self, addr: String) {
        *self.http_addr.lock().unwrap() = if addr.is_empty() { None } else { Some(addr) };
    }

    pub fn set_nameserver(&self, addr: String) {
        *self.nameserver.lock().unwrap() = addr;
    }

    /// Spawn (or queue) one mapping. While the node runs the listener starts
    /// immediately under its stats key; the mapping is always remembered so a
    /// later start() picks it up too.
    fn spawn_listener<M: Clone>(
        &self,
        listeners: &mut std::collections::HashMap<String, RunningListener>,
        netstack: &Arc<YggNetstack>,
        stats: &Arc<ListenerStatsRegistry>,
        mapping: M,
        spawn_fn: impl Fn(
            Arc<YggNetstack>,
            M,
            Arc<ListenerStatsRegistry>,
            tokio::sync::broadcast::Sender<()>,
        ) -> (String, tokio::task::JoinHandle<()>),
    ) {
        // The spawners call tokio::spawn, which panics outside a runtime
        // context. At start() we already run inside rt.block_on; the live
        // add_* entry points arrive on foreign (UniFFI) threads and must
        // enter the runtime explicitly.
        let _entered = if tokio::runtime::Handle::try_current().is_ok() {
            None
        } else {
            Some(self.rt.enter())
        };
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let (key, handle) = spawn_fn(netstack.clone(), mapping, stats.clone(), tx.clone());
        // Keep `rx` alive so a stop fired before the task first polls its own
        // subscription is not lost to a zero-receiver send.
        listeners.insert(
            key,
            RunningListener {
                _rx: rx,
                stop_tx: tx,
                handle,
            },
        );
    }

    pub fn add_local_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_local(&spec).map_err(YggstackError::Config)?;
        self.local_tcp.lock().unwrap_or_else(|e| e.into_inner()).push(m.clone());
        if let Some(node) = self.state.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let mut ls = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m, |ns, m, st, tx| {
                spawn_local_tcp(ns, m, tx, st)
            });
        }
        Ok(())
    }

    pub fn add_local_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_local(&spec).map_err(YggstackError::Config)?;
        self.local_udp.lock().unwrap_or_else(|e| e.into_inner()).push(m.clone());
        if let Some(node) = self.state.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let mut ls = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m, |ns, m, st, tx| {
                spawn_local_udp(ns, m, tx, st)
            });
        }
        Ok(())
    }

    pub fn add_remote_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_remote(&spec).map_err(YggstackError::Config)?;
        self.remote_tcp.lock().unwrap_or_else(|e| e.into_inner()).push(m.clone());
        if let Some(node) = self.state.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let mut ls = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m, |ns, m, st, tx| {
                spawn_remote_tcp(ns, m, tx, st)
            });
        }
        Ok(())
    }

    pub fn add_remote_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_remote(&spec).map_err(YggstackError::Config)?;
        self.remote_udp.lock().unwrap_or_else(|e| e.into_inner()).push(m.clone());
        if let Some(node) = self.state.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            let mut ls = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m, |ns, m, st, tx| {
                spawn_remote_udp(ns, m, tx, st)
            });
        }
        Ok(())
    }

    /// Remove a single local-tcp mapping. Stops its listener when the node is
    /// running; the mapping is dropped from the next start() either way.
    pub fn remove_local_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_local(&spec).map_err(YggstackError::Config)?;
        let key = local_tcp_key(&m);
        self.local_tcp
            .lock()
            .unwrap()
            .retain(|x| local_tcp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_local_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_local(&spec).map_err(YggstackError::Config)?;
        let key = local_udp_key(&m);
        self.local_udp
            .lock()
            .unwrap()
            .retain(|x| local_udp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_remote_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_remote(&spec).map_err(YggstackError::Config)?;
        let key = remote_tcp_key(&m);
        self.remote_tcp
            .lock()
            .unwrap()
            .retain(|x| remote_tcp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_remote_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_remote(&spec).map_err(YggstackError::Config)?;
        let key = remote_udp_key(&m);
        self.remote_udp
            .lock()
            .unwrap()
            .retain(|x| remote_udp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    fn stop_listener(&self, key: &str) {
        if let Some(mut l) = self.listeners.lock().unwrap_or_else(|e| e.into_inner()).remove(key) {
            let _ = l.stop_tx.send(());
            self.rt.block_on(join_listener(&mut l.handle));
        }
    }

    pub fn clear_mappings(&self) {
        self.local_tcp.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.local_udp.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.remote_tcp.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.remote_udp.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let listeners: Vec<RunningListener> = {
            let mut guard = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
            guard.drain().map(|(_, v)| v).collect()
        };
        for l in listeners {
            let _ = l.stop_tx.send(());
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).is_some()
    }

    pub fn add_live_peer(&self, uri: String) -> Result<(), YggstackError> {
        let core = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(|n| n.core.clone())
                .ok_or_else(|| YggstackError::NotRunning("not running".to_string()))?
        };
        self.rt
            .block_on(core.add_peer(&uri))
            .map_err(YggstackError::Runtime)
    }

    pub fn remove_live_peer(&self, uri: String) -> Result<(), YggstackError> {
        let core = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(|n| n.core.clone())
                .ok_or_else(|| YggstackError::NotRunning("not running".to_string()))?
        };
        self.rt
            .block_on(core.remove_peer(&uri))
            .map_err(YggstackError::Runtime)
    }

    pub fn retry_peers_now(&self) {
        let core = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|n| n.core.clone())
        };
        if let Some(core) = core {
            self.rt.block_on(core.retry_peers_now());
        }
    }

    /// Return a JSON array of per-listener connection/traffic stats in the
    /// same shape as the Go yggstack GetListenersJSON:
    ///   [{"Key","Kind","Listen","Target","ActiveConns","TotalConns",
    ///     "RXBytes","TXBytes"}]
    /// Returns "[]" when the node is not running.
    pub fn get_listeners_json(&self) -> String {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match guard.as_ref() {
            Some(node) => node.stats.to_json(),
            None => "[]".to_string(),
        }
    }

    /// Return a JSON array of connected peer stats compatible with the Android
    /// peer details panel. Each object has the fields:
    ///   URI, Up, Inbound, Port, Priority, Cost, RXBytes, TXBytes,
    ///   Uptime (nanoseconds), Latency (nanoseconds)
    /// Returns "[]" when the node is not running.
    pub fn get_peers_json(&self) -> String {
        let core = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard.as_ref().map(|n| n.core.clone())
        };
        let Some(core) = core else {
            return "[]".to_string();
        };
        self.rt.block_on(peers_snapshot_json(&core))
    }

    /// Start an ICMPv6 ping session to a Yggdrasil address (see the UDL for
    /// the event protocol). Replaces any running session.
    pub fn start_ping(
        &self,
        address: String,
        count: i32,
        timeout_ms: i64,
        interval_ms: i64,
        callback: Box<dyn PingCallback>,
    ) -> Result<(), YggstackError> {
        let dst: Ipv6Addr = address
            .trim()
            .trim_matches(|c| c == '[' || c == ']')
            .parse()
            .map_err(|_| YggstackError::Config(format!("invalid IPv6 address: {address}")))?;
        if !ping::is_ygg_address(&dst) {
            return Err(YggstackError::Config(
                "not a Yggdrasil address (must be within 200::/7)".to_string(),
            ));
        }
        let count = count.max(0) as u32;
        let timeout = Duration::from_millis(timeout_ms.clamp(50, 60_000) as u64);
        let interval = Duration::from_millis(interval_ms.clamp(0, 60_000) as u64);

        let netstack = {
            let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
            guard
                .as_ref()
                .map(|n| n.netstack.clone())
                .ok_or_else(|| YggstackError::NotRunning("not running".to_string()))?
        };

        // Replace a running session: join it so its final events are
        // delivered before this session starts probing.
        self.stop_ping();

        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let handle = self.rt.spawn(async move {
            let stop = watch_stop(stop_rx);
            ping::run_ping(&netstack, dst, count, timeout, interval, stop, move |ev| {
                callback.on_result(ping_event_json(ev));
            })
            .await;
        });
        *self.ping.lock().unwrap() = Some(PingSession { stop_tx, handle });
        Ok(())
    }

    /// Stop the running ping session, if any. Joins the task so no further
    /// callback events fire after this returns.
    pub fn stop_ping(&self) {
        let Some(mut session) = self.ping.lock().unwrap().take() else {
            return;
        };
        let _ = session.stop_tx.send(true);
        self.rt.block_on(async {
            if tokio::time::timeout(PING_STOP_GRACE, &mut session.handle)
                .await
                .is_err()
            {
                session.handle.abort();
                let _ = session.handle.await;
            }
        });
    }

    /// One-shot liveness probe of a remote Yggdrasil TCP service through
    /// the node's own netstack — no proxy or mapping involved. Returns
    /// "connected" (handshake completed: a listener answered), "refused"
    /// (RST: nothing listens on that port), "timeout" (no answer within
    /// timeout_ms) or "error" (node not running / malformed address).
    /// The dial is attempted twice with a short gap: on a cold start the
    /// overlay's first-contact path lookup (the signed tree-path
    /// discovery) and session handshake are still in flight while
    /// the first dial runs — the retry dials through the now-cached
    /// path and session instead of reporting a spurious red.
    pub fn check_tcp_port(&self, addr_port: String, timeout_ms: i64) -> String {
        let Some(addr) = parse_ygg_addr_port(&addr_port) else {
            return "error".to_string();
        };
        let netstack = match self.running_netstack() {
            Some(ns) => ns,
            None => return "error".to_string(),
        };
        let timeout = Duration::from_millis(timeout_ms.clamp(100, 15_000) as u64);
        // Layered windows: the first attempt only triggers the overlay's
        // first-contact path lookup and session handshake (a warm
        // destination answers well within a second);
        // the retry dials with the full window through the cached path.
        let first_window = timeout.min(Duration::from_secs(1));
        // The timeout future must be constructed INSIDE block_on: its Sleep
        // timer resolves the current runtime handle eagerly at creation,
        // which panics outside any runtime context.
        self.rt.block_on(async {
            let mut last = "timeout";
            for attempt in 0..2 {
                if attempt > 0 {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                let window = if attempt == 0 { first_window } else { timeout };
                match tokio::time::timeout(window, netstack.dial_tcp(addr)).await {
                    Ok(Ok(_)) => return "connected".to_string(),
                    // The stack surfaces a remote RST as ConnectionRefused:
                    // a real answer proving nothing listens on that port.
                    Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => {
                        last = "refused"
                    }
                    _ => last = "timeout",
                }
            }
            last.to_string()
        })
    }

    /// One-shot UDP probe: sends datagrams to a remote Yggdrasil UDP port
    /// and listens for any reply. UDP has no universal handshake, so silence
    /// is inconclusive rather than proof of death: returns "replied" (a
    /// service answered), "silent" (no reply within timeout_ms) or "error"
    /// (node not running / send failed / malformed address). The probe is
    /// re-sent across the window (the earliest datagrams are held back
    /// until the overlay's first-contact path lookup and session
    /// handshake complete); port 53 is probed with a real minimal DNS
    /// query, which generic zero-filled datagrams would be silently
    /// dropped for by most servers.
    pub fn check_udp_port(&self, addr_port: String, timeout_ms: i64) -> String {
        let Some(addr) = parse_ygg_addr_port(&addr_port) else {
            return "error".to_string();
        };
        let netstack = match self.running_netstack() {
            Some(ns) => ns,
            None => return "error".to_string(),
        };
        let timeout = Duration::from_millis(timeout_ms.clamp(100, 15_000) as u64);
        self.rt.block_on(async move {
            // Not open_udp(): ANY-bound sockets do not receive ingress in
            // this netstack — bind to the node's own address on a random
            // ephemeral port (retry on the rare collision).
            let sock = (0..3)
                .find_map(|_| {
                    let port = probe_ephemeral_port();
                    netstack.bind_udp(port).ok()
                })
                .or_else(|| netstack.open_udp().ok())?;
            let probe = udp_probe_payload(addr.port());
            // Full-size receive buffer: smoltcp's UDP recv errors with
            // Truncated when the datagram does not fit — any smaller buffer
            // would turn every real reply into an error.
            let mut buf = vec![0u8; 65_535];
            // Re-send the probe across the window (0 / 400ms / 1s / 1.8s):
            // on a cold start the earliest datagrams are held back by the
            // overlay's first-contact path lookup and session handshake —
            // TCP rides this out with SYN
            // retransmission, UDP has none. Any reply counts.
            let send_at = [
                Duration::from_millis(0),
                Duration::from_millis(400),
                Duration::from_millis(1000),
                Duration::from_millis(1800),
            ];
            let start = tokio::time::Instant::now();
            let mut sent = 0;
            loop {
                if sent < send_at.len() && start.elapsed() >= send_at[sent] {
                    sock.send_to(&probe, addr).await.ok()?;
                    sent += 1;
                }
                let next_event = if sent < send_at.len() {
                    send_at[sent]
                } else {
                    timeout
                }
                .saturating_sub(start.elapsed());
                tokio::select! {
                    r = sock.recv_from(&mut buf) => {
                        if r.is_ok() {
                            return Some("replied");
                        }
                        return None;
                    }
                    _ = tokio::time::sleep(next_event) => {
                        if start.elapsed() >= timeout {
                            return Some("silent");
                        }
                    }
                }
            }
        })
        .map(|r| r.to_string())
        .unwrap_or_else(|| "error".to_string())
    }

    /// The live netstack, or None when the node is not running.
    fn running_netstack(&self) -> Option<Arc<YggNetstack>> {
        let guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        guard.as_ref().map(|n| n.netstack.clone())
    }
}

impl Drop for YggstackMobile {
    fn drop(&mut self) {
        // Safety net for a dropped-without-stop engine: quiesce it, then tear
        // the runtime down with a bound so nothing outlives the object. The
        // app is expected to destroy the engine deterministically at stop()
        // (via the uniffi destroy()); relying on the JVM cleaner instead
        // frees the runtime at an arbitrary GC moment — racing a new engine
        // constructed in the same process (native crash on start-after-stop).
        self.stop();
        let placeholder = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to create placeholder runtime");
        let rt = std::mem::replace(&mut self.rt, placeholder);
        rt.shutdown_timeout(SHUTDOWN_GRACE);
    }
}

#[cfg(test)]
mod port_check_tests {
    use super::*;

    // The port checks must verify a real service on the remote side, dialed
    // through the node's own netstack. An offline node plus its own remote
    // (exposed) mappings make a self-contained remote side: a dial to the
    // node's Yggdrasil address lands on the exposed port, forwarded to the
    // loopback service.

    fn started_node() -> (YggstackMobile, String) {
        let y = YggstackMobile::new();
        y.generate_and_load_config().expect("config");
        y.start().expect("start");
        let addr = y.get_address().expect("address");
        (y, addr)
    }

    #[test]
    fn udp_probe_payload_dns() {
        let dns = udp_probe_payload(53);
        assert_eq!(dns.len(), 17, "minimal root NS query is 17 bytes");
        assert_eq!(&dns[..2], &[0x12, 0x34]);
        assert_eq!(dns[2], 0x01, "RD flag set");
        assert_eq!(udp_probe_payload(5353), vec![0u8; 16]);
    }

    #[test]
    fn live_mapping_toggle_outside_runtime_context() {
        let (y, addr) = started_node();
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in ln.incoming() {
                drop(c);
            }
        });
        // Live entry points arrive on UniFFI threads with no runtime
        // context: both must enter the runtime internally instead of
        // panicking ("there is no reactor running") and poisoning the
        // state lock (regression: ng could not enable forwards on the
        // fly, and stop() then failed with PoisonError).
        y.add_remote_tcp(format!("{port}:127.0.0.1:{port}")).expect("live expose");
        y.add_local_tcp(format!("127.0.0.1:{port}:[{addr}]:{port}")).expect("live forward");

        // Full data path through the live-added forward
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "live-added forward never connected"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        // The port check runs on this same foreign thread
        assert_eq!(y.check_tcp_port(format!("[{addr}]:{port}"), 2000), "connected");
        y.stop();
    }

    #[test]
    fn check_tcp_port_outcomes() {
        let (y, addr) = started_node();

        // A real remote service: loopback listener exposed via the node
        let ln = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = ln.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in ln.incoming() {
                drop(c); // handshake completes; nothing to serve
            }
        });
        // add_remote_* must run inside the runtime context (its listener
        // spawns via tokio::spawn) — the same context the start() path
        // provides in production.
        y.rt
            .block_on(async { y.add_remote_tcp(format!("{port}:127.0.0.1:{port}")).expect("mapping") });

        let target = format!("[{addr}]:{port}");
        let mut got = String::new();
        for _ in 0..50 {
            got = y.check_tcp_port(target.clone(), 1000);
            if got == "connected" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(got, "connected", "exposed port must answer");

        // Closed port on the node's own address: the stack answers RST
        assert_eq!(y.check_tcp_port(format!("[{addr}]:1"), 1000), "refused");

        y.stop();
        assert_eq!(y.check_tcp_port(target, 500), "error");
    }

    #[test]
    fn check_tcp_port_rejects_non_ygg_address() {
        let (y, _) = started_node();
        assert_eq!(y.check_tcp_port("127.0.0.1:80".into(), 500), "error");
        assert_eq!(y.check_tcp_port("not-an-address".into(), 500), "error");
        y.stop();
    }

    #[test]
    fn check_udp_port_outcomes() {
        let (y, addr) = started_node();

        // A replying UDP service behind an exposed port
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            loop {
                let Ok((n, from)) = sock.recv_from(&mut buf) else { return };
                if sock.send_to(&buf[..n], from).is_err() {
                    return;
                }
            }
        });
        y.rt
            .block_on(async { y.add_remote_udp(format!("{port}:127.0.0.1:{port}")).expect("mapping") });

        let target = format!("[{addr}]:{port}");
        let mut got = String::new();
        for _ in 0..50 {
            got = y.check_udp_port(target.clone(), 1000);
            if got == "replied" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(got, "replied", "exposed UDP port must echo, got={got}");

        // Closed UDP port: no answer — inconclusive, "silent" (never dead)
        assert_eq!(y.check_udp_port(format!("[{addr}]:1"), 300), "silent");

        y.stop();
        assert_eq!(y.check_udp_port(target, 500), "error");
    }
}
