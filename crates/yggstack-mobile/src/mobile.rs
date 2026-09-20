//! UniFFI mobile bindings for yggstack.
use std::sync::{Arc, Mutex, Once};

use yggdrasil::core::Core;
use yggdrasil::ipv6rwc::ReadWriteCloser;

use yggstack::config;
use yggstack::forward::tcp::{local_tcp_key, remote_tcp_key, spawn_local_tcp, spawn_remote_tcp};
use yggstack::forward::udp::{local_udp_key, remote_udp_key, spawn_local_udp, spawn_remote_udp};
use yggstack::mapping::{TcpMapping, UdpMapping};
use yggstack::netstack::YggNetstack;
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

async fn join_listener(handle: &mut tokio::task::JoinHandle<()>) {
    if tokio::time::timeout(LISTENER_STOP_GRACE, &mut *handle).await.is_err() {
        handle.abort();
        let _ = handle.await;
    }
}

// ── YggstackMobile ────────────────────────────────────────────────────────────

pub struct YggstackMobile {
    rt: Arc<tokio::runtime::Runtime>,
    state: Mutex<Option<NodeState>>,
    cfg: Mutex<Option<yggdrasil::config::Config>>,
    socks_addr: Mutex<Option<String>>,
    nameserver: Mutex<String>,
    local_tcp: Mutex<Vec<TcpMapping>>,
    local_udp: Mutex<Vec<UdpMapping>>,
    remote_tcp: Mutex<Vec<TcpMapping>>,
    remote_udp: Mutex<Vec<UdpMapping>>,
    /// Live listeners for the current run, keyed by the same stats key the
    /// forwarders register under. Empty when the node is not running.
    listeners: Mutex<std::collections::HashMap<String, RunningListener>>,
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
            rt: Arc::new(rt),
            state: Mutex::new(None),
            cfg: Mutex::new(None),
            socks_addr: Mutex::new(None),
            nameserver: Mutex::new(String::new()),
            local_tcp: Mutex::new(Vec::new()),
            local_udp: Mutex::new(Vec::new()),
            remote_tcp: Mutex::new(Vec::new()),
            remote_udp: Mutex::new(Vec::new()),
            listeners: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn set_log_callback(&self, callback: Box<dyn LogCallback>) {
        *LOG_SINK.lock().unwrap() = Some(callback);
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
        let mut cfg: yggdrasil::config::Config = toml::from_str(&toml_config)
            .map_err(|e| YggstackError::Config(e.to_string()))?;
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
        let key = cfg
            .signing_key()
            .map_err(YggstackError::Config)?;
        let pk = key.verifying_key().to_bytes();
        Ok(config::addr_for_key(&pk).to_string())
    }

    pub fn get_subnet(&self) -> Result<String, YggstackError> {
        let guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_ref()
            .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?;
        let key = cfg
            .signing_key()
            .map_err(YggstackError::Config)?;
        let pk = key.verifying_key().to_bytes();
        let (ip, pfx) = config::subnet_for_key(&pk);
        Ok(format!("{}/{}", ip, pfx))
    }

    pub fn get_public_key(&self) -> Result<String, YggstackError> {
        let guard = self.cfg.lock().unwrap();
        let cfg = guard
            .as_ref()
            .ok_or_else(|| YggstackError::Config("no config loaded".to_string()))?;
        let key = cfg
            .signing_key()
            .map_err(YggstackError::Config)?;
        Ok(hex::encode(key.verifying_key().to_bytes()))
    }

    pub fn start(&self) -> Result<(), YggstackError> {
        {
            let guard = self.state.lock().unwrap();
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

        let signing_key = cfg
            .signing_key()
            .map_err(YggstackError::Config)?;

        let socks_addr = self.socks_addr.lock().unwrap().clone();
        let nameserver = self.nameserver.lock().unwrap().clone();
        let local_tcp = self.local_tcp.lock().unwrap().clone();
        let local_udp = self.local_udp.lock().unwrap().clone();
        let remote_tcp = self.remote_tcp.lock().unwrap().clone();
        let remote_udp = self.remote_udp.lock().unwrap().clone();

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

            let mut listeners = self.listeners.lock().unwrap();
            for m in local_tcp  { self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| spawn_local_tcp(ns, m, tx, st));  }
            for m in local_udp  { self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| spawn_local_udp(ns, m, tx, st));  }
            for m in remote_tcp { self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| spawn_remote_tcp(ns, m, tx, st)); }
            for m in remote_udp { self.spawn_listener(&mut listeners, &netstack, &stats, m, |ns, m, st, tx| spawn_remote_udp(ns, m, tx, st)); }

            NodeState {
                core,
                _rwc: rwc,
                netstack,
                stop_tx,
                stats,
                socks_handle,
            }
        });

        *self.state.lock().unwrap() = Some(node);
        Ok(())
    }

    pub fn stop(&self) {
        let node = { self.state.lock().unwrap().take() };
        if let Some(node) = node {
            self.rt.block_on(node.core.close_multicast());
            // Close links + the ironwood core as well. Without this the old
            // instance keeps its peer connections alive under the same node
            // identity; after a restart, remote peers may keep routing
            // inbound traffic over the dead instance's links until they hit
            // their idle/keepalive timeout (observed as a 1-2 minute stall
            // of all forwarded connections after a stop/start cycle).
            let _ = self.rt.block_on(node.core.close());
            let _ = node.stop_tx.send(());
            // Join the SOCKS server so the proxy port is released before
            // stop() returns (Go joins all workers in Stop()).
            if let Some(mut socks) = node.socks_handle {
                self.rt.block_on(join_listener(&mut socks));
            }
        }
        // Stop and JOIN every per-mapping listener so their listening
        // sockets are closed before stop() returns — an immediate restart
        // can then rebind the same ports without racing the old tasks.
        let listeners: Vec<RunningListener> = {
            let mut guard = self.listeners.lock().unwrap();
            guard.drain().map(|(_, v)| v).collect()
        };
        let mut handles = Vec::new();
        for mut l in listeners {
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
        spawn_fn: impl Fn(Arc<YggNetstack>, M, Arc<ListenerStatsRegistry>, tokio::sync::broadcast::Sender<()>) -> (String, tokio::task::JoinHandle<()>),
    ) {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        let (key, handle) = spawn_fn(netstack.clone(), mapping, stats.clone(), tx.clone());
        // Keep `rx` alive so a stop fired before the task first polls its own
        // subscription is not lost to a zero-receiver send.
        listeners.insert(key, RunningListener { _rx: rx, stop_tx: tx, handle });
    }

    pub fn add_local_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_local(&spec)
            .map_err(YggstackError::Config)?;
        self.local_tcp.lock().unwrap().push(m.clone());
        if let Some(node) = self.state.lock().unwrap().as_ref() {
            let mut ls = self.listeners.lock().unwrap();
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m,
                |ns, m, st, tx| spawn_local_tcp(ns, m, tx, st));
        }
        Ok(())
    }

    pub fn add_local_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_local(&spec)
            .map_err(YggstackError::Config)?;
        self.local_udp.lock().unwrap().push(m.clone());
        if let Some(node) = self.state.lock().unwrap().as_ref() {
            let mut ls = self.listeners.lock().unwrap();
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m,
                |ns, m, st, tx| spawn_local_udp(ns, m, tx, st));
        }
        Ok(())
    }

    pub fn add_remote_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_remote(&spec)
            .map_err(YggstackError::Config)?;
        self.remote_tcp.lock().unwrap().push(m.clone());
        if let Some(node) = self.state.lock().unwrap().as_ref() {
            let mut ls = self.listeners.lock().unwrap();
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m,
                |ns, m, st, tx| spawn_remote_tcp(ns, m, tx, st));
        }
        Ok(())
    }

    pub fn add_remote_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_remote(&spec)
            .map_err(YggstackError::Config)?;
        self.remote_udp.lock().unwrap().push(m.clone());
        if let Some(node) = self.state.lock().unwrap().as_ref() {
            let mut ls = self.listeners.lock().unwrap();
            self.spawn_listener(&mut ls, &node.netstack, &node.stats, m,
                |ns, m, st, tx| spawn_remote_udp(ns, m, tx, st));
        }
        Ok(())
    }

    /// Remove a single local-tcp mapping. Stops its listener when the node is
    /// running; the mapping is dropped from the next start() either way.
    pub fn remove_local_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_local(&spec)
            .map_err(YggstackError::Config)?;
        let key = local_tcp_key(&m);
        self.local_tcp.lock().unwrap().retain(|x| local_tcp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_local_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_local(&spec)
            .map_err(YggstackError::Config)?;
        let key = local_udp_key(&m);
        self.local_udp.lock().unwrap().retain(|x| local_udp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_remote_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_remote(&spec)
            .map_err(YggstackError::Config)?;
        let key = remote_tcp_key(&m);
        self.remote_tcp.lock().unwrap().retain(|x| remote_tcp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    pub fn remove_remote_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_remote(&spec)
            .map_err(YggstackError::Config)?;
        let key = remote_udp_key(&m);
        self.remote_udp.lock().unwrap().retain(|x| remote_udp_key(x) != key);
        self.stop_listener(&key);
        Ok(())
    }

    fn stop_listener(&self, key: &str) {
        if let Some(mut l) = self.listeners.lock().unwrap().remove(key) {
            let _ = l.stop_tx.send(());
            self.rt.block_on(join_listener(&mut l.handle));
        }
    }

    pub fn clear_mappings(&self) {
        self.local_tcp.lock().unwrap().clear();
        self.local_udp.lock().unwrap().clear();
        self.remote_tcp.lock().unwrap().clear();
        self.remote_udp.lock().unwrap().clear();
        let listeners: Vec<RunningListener> = {
            let mut guard = self.listeners.lock().unwrap();
            guard.drain().map(|(_, v)| v).collect()
        };
        for l in listeners {
            let _ = l.stop_tx.send(());
        }
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    pub fn add_live_peer(&self, uri: String) -> Result<(), YggstackError> {
        let core = {
            let guard = self.state.lock().unwrap();
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
            let guard = self.state.lock().unwrap();
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
            let guard = self.state.lock().unwrap();
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
        let guard = self.state.lock().unwrap();
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
            let guard = self.state.lock().unwrap();
            guard.as_ref().map(|n| n.core.clone())
        };
        let Some(core) = core else {
            return "[]".to_string();
        };
        let peers = self.rt.block_on(core.get_peers());
        let items: Vec<String> = peers
            .iter()
            .map(|p| {
                let uri_json =
                    serde_json::to_string(&p.uri).unwrap_or_else(|_| "\"\"".to_string());
                let port = p.uri.split('?').next()
                    .and_then(|u| u.rsplit(':').next())
                    .and_then(|p| p.parse::<u16>().ok())
                    .unwrap_or(0);
                format!(
                    r#"{{"URI":{uri},"Up":{up},"Inbound":{inbound},"Port":{port},"Priority":{prio},"Cost":{cost},"RXBytes":{rx},"TXBytes":{tx},"Uptime":{uptime:.0},"Latency":{latency:.0}}}"#,
                    uri = uri_json,
                    up = p.up,
                    inbound = p.inbound,
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
}
