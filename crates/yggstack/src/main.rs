mod config;
mod forward;
mod netstack;
mod socks;

use std::sync::Arc;

use clap::Parser;
use ed25519_dalek::SigningKey;
use tracing_subscriber::EnvFilter;

use yggdrasil::config::Config as YggConfig;
use yggdrasil::core::Core;
use yggdrasil::ipv6rwc::ReadWriteCloser;

use crate::config::{generate_config_text, parse_local_mapping, parse_remote_mapping, Config};
use crate::netstack::create_netstack;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "yggstack",
    version,
    about = "Yggdrasil as SOCKS5 proxy / port forwarder (Rust)"
)]
struct Args {
    /// Config file path
    #[arg(short = 'c', long, value_name = "FILE")]
    config: Option<String>,

    /// Generate a skeleton config (optionally to FILE) and exit
    #[arg(long, value_name = "FILE", num_args = 0..=1)]
    genconf: Option<Option<String>>,

    /// Run with ephemeral keys (no config file required)
    #[arg(long)]
    autoconf: bool,

    /// Print the node's Yggdrasil IPv6 address and exit
    #[arg(long)]
    address: bool,

    /// Log level: error / warn / info / debug / trace
    #[arg(short = 'l', long, default_value = "info")]
    loglevel: String,

    /// SOCKS5 listen address, e.g. 127.0.0.1:1080 (overrides config)
    #[arg(long, value_name = "ADDR")]
    socks: Option<String>,

    /// DNS name-server for SOCKS5 hostname resolution
    #[arg(long, value_name = "ADDR")]
    nameserver: Option<String>,

    /// Local TCP forward: [bind_addr:]bind_port:[ygg_addr]:ygg_port
    #[arg(long = "local-tcp", value_name = "MAPPING")]
    local_tcp: Vec<String>,

    /// Local UDP forward: [bind_addr:]bind_port:[ygg_addr]:ygg_port
    #[arg(long = "local-udp", value_name = "MAPPING")]
    local_udp: Vec<String>,

    /// Remote TCP forward: ygg_port[:local_addr:local_port]
    #[arg(long = "remote-tcp", value_name = "MAPPING")]
    remote_tcp: Vec<String>,

    /// Remote UDP forward: ygg_port[:local_addr:local_port]
    #[arg(long = "remote-udp", value_name = "MAPPING")]
    remote_udp: Vec<String>,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // ── genconf ───────────────────────────────────────────────────────────────
    if let Some(path_opt) = &args.genconf {
        let text = generate_config_text();
        match path_opt {
            Some(path) => {
                std::fs::write(path, &text)?;
                eprintln!("Configuration written to {}", path);
            }
            None => print!("{}", text),
        }
        return Ok(());
    }

    // ── logging ───────────────────────────────────────────────────────────────
    let filter = EnvFilter::try_new(&args.loglevel)
        .unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(true)
        .init();

    // ── Load config ───────────────────────────────────────────────────────────
    let mut cfg: Config = if args.autoconf {
        Config::default()
    } else if let Some(path) = &args.config {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("read config {}: {}", path, e))?;
        toml::from_str(&text)
            .map_err(|e| format!("parse config {}: {}", path, e))?
    } else {
        // Try default file "yggstack.toml"
        match std::fs::read_to_string("yggstack.toml") {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| format!("parse yggstack.toml: {}", e))?,
            Err(_) => {
                eprintln!(
                    "No config file found. Use --genconf to generate one, \
                     --config to specify a file, or --autoconf for ephemeral mode."
                );
                std::process::exit(1);
            }
        }
    };

    // Yggstack never uses a TUN adapter
    cfg.yggdrasil.if_name = "none".to_string();

    // ── CLI overrides ─────────────────────────────────────────────────────────
    if let Some(socks) = args.socks {
        cfg.yggstack.socks = Some(socks);
    }
    if let Some(ns) = args.nameserver {
        cfg.yggstack.nameserver = Some(ns);
    }
    cfg.yggstack.local_tcp.extend(args.local_tcp);
    cfg.yggstack.local_udp.extend(args.local_udp);
    cfg.yggstack.remote_tcp.extend(args.remote_tcp);
    cfg.yggstack.remote_udp.extend(args.remote_udp);

    // ── Signing key ───────────────────────────────────────────────────────────
    let signing_key: SigningKey = if !cfg.yggdrasil.private_key.is_empty() {
        cfg.yggdrasil
            .signing_key()
            .map_err(|e| format!("invalid private key: {}", e))?
    } else if let Ok(env_key) = std::env::var("YGGDRASIL_PRIVATE_KEY") {
        let bytes = hex::decode(&env_key)
            .map_err(|e| format!("invalid YGGDRASIL_PRIVATE_KEY hex: {}", e))?;
        let arr: [u8; 64] = bytes
            .try_into()
            .map_err(|v: Vec<u8>| format!("YGGDRASIL_PRIVATE_KEY: expected 64 bytes, got {}", v.len()))?;
        SigningKey::from_keypair_bytes(&arr)
            .map_err(|e| format!("invalid ed25519 key: {}", e))?
    } else {
        tracing::warn!("No private key set – generating ephemeral key");
        SigningKey::generate(&mut rand::rngs::OsRng)
    };

    // ── --address ─────────────────────────────────────────────────────────────
    if args.address {
        let pub_key = signing_key.verifying_key().to_bytes();
        let addr = yggdrasil::address::addr_for_key(&pub_key);
        println!("{}", addr);
        return Ok(());
    }

    // ── Start Yggdrasil core ──────────────────────────────────────────────────
    let ygg_cfg: YggConfig = cfg.yggdrasil.clone();
    let core = Core::new(signing_key, ygg_cfg);

    let our_addr = *core.address();
    let our_addr_bytes = our_addr.0;

    tracing::info!("IPv6 address: {}", our_addr);
    tracing::info!("IPv6 subnet:  {}", core.subnet());
    tracing::info!("Public key:   {}", hex::encode(core.public_key()));

    core.init_links().await;
    core.start().await;

    // ── Build ReadWriteCloser + Netstack ──────────────────────────────────────
    let mtu = core.mtu();
    let rwc = ReadWriteCloser::new(core.clone(), mtu);
    core.set_path_notify(rwc.clone());

    let netstack = Arc::new(create_netstack(rwc, our_addr_bytes));

    // ── Parse mappings ────────────────────────────────────────────────────────
    let our_addr_str = our_addr.to_string();

    let local_tcp_mappings = parse_mappings_local(&cfg.yggstack.local_tcp, "local-tcp")?;
    let local_udp_mappings = parse_mappings_local(&cfg.yggstack.local_udp, "local-udp")?;
    let remote_tcp_mappings =
        parse_mappings_remote(&cfg.yggstack.remote_tcp, &our_addr_str, "remote-tcp")?;
    let remote_udp_mappings =
        parse_mappings_remote(&cfg.yggstack.remote_udp, &our_addr_str, "remote-udp")?;

    // ── Spawn forwarding tasks ────────────────────────────────────────────────

    // Local TCP
    for (listen, remote) in local_tcp_mappings {
        let ns = netstack.clone();
        tokio::spawn(async move {
            if let Err(e) = forward::local_tcp(listen, remote, ns).await {
                tracing::error!("local-tcp {}: {}", listen, e);
            }
        });
    }

    // Local UDP
    for (listen, remote) in local_udp_mappings {
        let ns = netstack.clone();
        tokio::spawn(async move {
            if let Err(e) = forward::local_udp_v2(listen, remote, ns).await {
                tracing::error!("local-udp {}: {}", listen, e);
            }
        });
    }

    // Remote TCP
    for (ygg_listen, local_target) in remote_tcp_mappings {
        let ns = netstack.clone();
        tokio::spawn(async move {
            if let Err(e) = forward::remote_tcp(ygg_listen.port(), local_target, ns).await {
                tracing::error!("remote-tcp port {}: {}", ygg_listen.port(), e);
            }
        });
    }

    // Remote UDP
    for (ygg_listen, local_target) in remote_udp_mappings {
        let ns = netstack.clone();
        tokio::spawn(async move {
            if let Err(e) = forward::remote_udp(ygg_listen.port(), local_target, ns).await {
                tracing::error!("remote-udp port {}: {}", ygg_listen.port(), e);
            }
        });
    }

    // SOCKS5
    if let Some(ref socks_addr) = cfg.yggstack.socks.clone() {
        let ns = netstack.clone();
        let addr = socks_addr.clone();
        let nameserver: Option<std::net::SocketAddr> = cfg
            .yggstack
            .nameserver
            .as_deref()
            .and_then(|s| s.parse().ok());
        if cfg.yggstack.nameserver.is_some() && nameserver.is_none() {
            tracing::warn!(
                "could not parse nameserver '{}' as socket address",
                cfg.yggstack.nameserver.as_deref().unwrap_or("")
            );
        }
        tokio::spawn(async move {
            if let Err(e) = socks::serve_socks5(&addr, ns, nameserver).await {
                tracing::error!("socks5: {}", e);
            }
        });
    }

    // ── Wait for Ctrl-C ───────────────────────────────────────────────────────
    tracing::info!("yggstack running. Press Ctrl-C to stop.");
    tokio::signal::ctrl_c().await?;
    tracing::info!("Shutting down…");
    core.close().await.ok();

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_mappings_local(
    specs: &[String],
    label: &str,
) -> Result<Vec<(std::net::SocketAddr, std::net::SocketAddr)>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for s in specs {
        let (listen, remote) = parse_local_mapping(s)
            .map_err(|e| format!("{} mapping '{}': {}", label, s, e))?;
        out.push((listen, remote));
    }
    Ok(out)
}

fn parse_mappings_remote(
    specs: &[String],
    our_addr: &str,
    label: &str,
) -> Result<Vec<(std::net::SocketAddr, std::net::SocketAddr)>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    for s in specs {
        let (listen, remote) = parse_remote_mapping(s, our_addr)
            .map_err(|e| format!("{} mapping '{}': {}", label, s, e))?;
        out.push((listen, remote));
    }
    Ok(out)
}
