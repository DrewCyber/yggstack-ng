//! UniFFI mobile bindings for yggstack.
use std::sync::{Arc, Mutex, Once};

use yggdrasil::core::Core;
use yggdrasil::ipv6rwc::ReadWriteCloser;

use yggstack::config;
use yggstack::forward::tcp::{spawn_local_tcp, spawn_remote_tcp};
use yggstack::forward::udp::{spawn_local_udp, spawn_remote_udp};
use yggstack::mapping::{TcpMapping, UdpMapping};
use yggstack::netstack::YggNetstack;
use yggstack::resolver::NameResolver;
use yggstack::socks::Socks5Server;

// ── Tracing init ──────────────────────────────────────────────────────────────

fn init_tracing() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::EnvFilter;

        let filter = EnvFilter::new("yggstack=info,yggdrasil=info,ironwood=warn");

        #[cfg(target_os = "android")]
        {
            tracing_subscriber::registry()
                .with(filter)
                .with(tracing_android::layer("yggstack").unwrap())
                .init();
        }
        #[cfg(not(target_os = "android"))]
        {
            tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer())
                .init();
        }
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

// ── Running node state ────────────────────────────────────────────────────────

struct NodeState {
    core: Arc<Core>,
    _rwc: Arc<ReadWriteCloser>,
    _netstack: Arc<YggNetstack>,
    stop_tx: tokio::sync::broadcast::Sender<()>,
}

// ── YggstackMobile ────────────────────────────────────────────────────────────

pub struct YggstackMobile {
    rt: Arc<tokio::runtime::Runtime>,
    state: Mutex<Option<NodeState>>,
    cfg: Mutex<Option<yggdrasil::config::Config>>,
    log_callback: Mutex<Option<Box<dyn LogCallback>>>,
    log_level: Mutex<String>,
    socks_addr: Mutex<Option<String>>,
    nameserver: Mutex<String>,
    local_tcp: Mutex<Vec<TcpMapping>>,
    local_udp: Mutex<Vec<UdpMapping>>,
    remote_tcp: Mutex<Vec<TcpMapping>>,
    remote_udp: Mutex<Vec<UdpMapping>>,
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
            log_callback: Mutex::new(None),
            log_level: Mutex::new("info".to_string()),
            socks_addr: Mutex::new(None),
            nameserver: Mutex::new(String::new()),
            local_tcp: Mutex::new(Vec::new()),
            local_udp: Mutex::new(Vec::new()),
            remote_tcp: Mutex::new(Vec::new()),
            remote_udp: Mutex::new(Vec::new()),
        }
    }

    pub fn set_log_callback(&self, callback: Box<dyn LogCallback>) {
        *self.log_callback.lock().unwrap() = Some(callback);
    }

    pub fn set_log_level(&self, level: String) {
        *self.log_level.lock().unwrap() = level;
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
            .map_err(|e| YggstackError::Config(e))?;
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
            .map_err(|e| YggstackError::Config(e))?;
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
            .map_err(|e| YggstackError::Config(e))?;
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
            .map_err(|e| YggstackError::Config(e))?;

        let socks_addr = self.socks_addr.lock().unwrap().clone();
        let nameserver = self.nameserver.lock().unwrap().clone();
        let local_tcp = self.local_tcp.lock().unwrap().clone();
        let local_udp = self.local_udp.lock().unwrap().clone();
        let remote_tcp = self.remote_tcp.lock().unwrap().clone();
        let remote_udp = self.remote_udp.lock().unwrap().clone();

        let node = self.rt.block_on(async {
            let pk = signing_key.verifying_key().to_bytes();
            let our_addr = config::addr_for_key(&pk);
            let core = Core::new(signing_key, cfg);
            core.init_links().await;
            core.start().await;
            let mtu = core.mtu();
            let rwc = ReadWriteCloser::new(core.clone(), mtu);
            core.set_path_notify(rwc.clone());

            let netstack = YggNetstack::new(rwc.clone(), our_addr, mtu);

            let resolver = Arc::new(NameResolver::new(netstack.clone(), &nameserver));

            if let Some(addr) = socks_addr {
                let srv = Arc::new(Socks5Server::new(netstack.clone(), resolver.clone()));
                let a2 = addr.clone();
                tokio::spawn(async move {
                    if let Err(e) = srv.serve_tcp(&a2).await {
                        tracing::error!("SOCKS5: {}", e);
                    }
                });
            }

            for m in local_tcp  { spawn_local_tcp(netstack.clone(), m);  }
            for m in local_udp  { spawn_local_udp(netstack.clone(), m);  }
            for m in remote_tcp { spawn_remote_tcp(netstack.clone(), m); }
            for m in remote_udp { spawn_remote_udp(netstack.clone(), m); }

            let (stop_tx, _) = tokio::sync::broadcast::channel(1);
            NodeState {
                core,
                _rwc: rwc,
                _netstack: netstack,
                stop_tx,
            }
        });

        *self.state.lock().unwrap() = Some(node);
        Ok(())
    }

    pub fn stop(&self) {
        let mut guard = self.state.lock().unwrap();
        if let Some(node) = guard.take() {
            let _ = node.stop_tx.send(());
        }
    }

    pub fn set_socks(&self, addr: String) {
        *self.socks_addr.lock().unwrap() = if addr.is_empty() { None } else { Some(addr) };
    }

    pub fn set_nameserver(&self, addr: String) {
        *self.nameserver.lock().unwrap() = addr;
    }

    pub fn add_local_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_local(&spec)
            .map_err(|e| YggstackError::Config(e))?;
        self.local_tcp.lock().unwrap().push(m);
        Ok(())
    }

    pub fn add_local_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_local(&spec)
            .map_err(|e| YggstackError::Config(e))?;
        self.local_udp.lock().unwrap().push(m);
        Ok(())
    }

    pub fn add_remote_tcp(&self, spec: String) -> Result<(), YggstackError> {
        let m = TcpMapping::parse_remote(&spec)
            .map_err(|e| YggstackError::Config(e))?;
        self.remote_tcp.lock().unwrap().push(m);
        Ok(())
    }

    pub fn add_remote_udp(&self, spec: String) -> Result<(), YggstackError> {
        let m = UdpMapping::parse_remote(&spec)
            .map_err(|e| YggstackError::Config(e))?;
        self.remote_udp.lock().unwrap().push(m);
        Ok(())
    }

    pub fn clear_mappings(&self) {
        self.local_tcp.lock().unwrap().clear();
        self.local_udp.lock().unwrap().clear();
        self.remote_tcp.lock().unwrap().clear();
        self.remote_udp.lock().unwrap().clear();
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
            .map_err(|e| YggstackError::Runtime(e))
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
            .map_err(|e| YggstackError::Runtime(e))
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
}
