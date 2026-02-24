use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Top-level yggstack config file.  The `[yggdrasil]` table is forwarded
/// directly to the upstream library; the `[yggstack]` table contains all
/// port-forwarding/proxy settings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub yggdrasil: yggdrasil::config::Config,

    #[serde(default)]
    pub yggstack: YggstackConfig,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct YggstackConfig {
    /// SOCKS5 listen address, e.g. "127.0.0.1:1080" or "/tmp/yggstack.sock"
    #[serde(default)]
    pub socks: Option<String>,

    /// DNS name-server to use for SOCKS domain resolution
    #[serde(default)]
    pub nameserver: Option<String>,

    /// Local TCP port forwardings: "127.0.0.1:8080:[::1]:80" or "8080:[::1]:80"
    #[serde(default)]
    pub local_tcp: Vec<String>,

    /// Local UDP port forwardings: "127.0.0.1:5553:[rem]:53" or "5553:[rem]:53"
    #[serde(default)]
    pub local_udp: Vec<String>,

    /// Remote TCP forwardings: "80:127.0.0.1:8080" or "80"
    #[serde(default)]
    pub remote_tcp: Vec<String>,

    /// Remote UDP forwardings: "53:127.0.0.1:53" or "53"
    #[serde(default)]
    pub remote_udp: Vec<String>,
}

// ── Parsing helpers ───────────────────────────────────────────────────────────

/// Parse a local-TCP/UDP mapping spec.
///
/// Accepted formats (matching yggstack CLI style):
///   `[bind_host:]bind_port:[target_host]:target_port`
///
/// Examples:
///   `127.0.0.1:5553:[308:62:45:62::]:53`
///   `5553:[308:62:45:62::]:53`
pub fn parse_local_mapping(spec: &str) -> Result<(SocketAddr, SocketAddr), String> {
    // Strategy: find the last ']' (for IPv6 target) and split from there.
    // Format variations handled by trying heuristics.

    // Remove optional outer brackets around IPv6 in bind position
    let spec = spec.trim();

    // Find the target part – it always ends with `]:<port>` (IPv6) or
    // `host:port` (IPv4/bare).  We split by identifying the target host:port
    // suffix.

    // Work backwards to find "]:<port>" or "host:port" target
    let (listen_part, target_part) = split_mapping(spec)?;

    let listen_addr = parse_addr_with_default_host(listen_part, "127.0.0.1")?;
    let target_addr = parse_addr_strict(target_part)?;
    Ok((listen_addr, target_addr))
}

/// Parse a remote-TCP/UDP mapping spec.
///
/// Accepted formats:
///   `ygg_port`                         (listen on all ygg addrs, forward to 127.0.0.1:ygg_port)
///   `ygg_port:local_host:local_port`
///   `ygg_port:local_port`
pub fn parse_remote_mapping(spec: &str, our_addr: &str) -> Result<(SocketAddr, SocketAddr), String> {
    let spec = spec.trim();

    // Try splitting on ':'
    // First token is always the ygg listen port
    let first_colon = spec.find(':');

    let our_listen = format!("[{}]", our_addr);

    let (ygg_port_str, rest) = match first_colon {
        None => (spec, ""),
        Some(i) => (&spec[..i], &spec[i + 1..]),
    };

    let ygg_port: u16 = ygg_port_str
        .parse()
        .map_err(|_| format!("invalid port '{}'", ygg_port_str))?;

    let listen_addr: SocketAddr = format!("{}:{}", our_listen, ygg_port)
        .parse()
        .map_err(|e| format!("build listen addr: {}", e))?;

    let target_addr = if rest.is_empty() {
        // Forward to 127.0.0.1:<same port>
        format!("127.0.0.1:{}", ygg_port)
            .parse()
            .map_err(|e: std::net::AddrParseError| e.to_string())?
    } else {
        // rest is "host:port" or ":port"
        parse_addr_with_default_host(rest, "127.0.0.1")?
    };

    Ok((listen_addr, target_addr))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Split `[bind]:port:[target]:port` style spec into (bind_part, target_part).
fn split_mapping(spec: &str) -> Result<(&str, &str), String> {
    // Find the last occurrence of ":[remote]:" or ":bare_host:" that starts the
    // target section.  The target is always the LAST `host:port` pair.

    // If spec contains "]:" we have an IPv6 target.
    // Pattern: everything before the last ":[" is the listen part.
    if let Some(bracket_pos) = spec.rfind(":[") {
        // listen part is spec[..bracket_pos], target part starts at bracket_pos+1
        let listen = &spec[..bracket_pos];
        let target = &spec[bracket_pos + 1..];
        return Ok((listen, target));
    }

    // No IPv6 brackets – split by last ':'  to get port, then the part before
    // it by last ':' is the host, what remains is the listen spec.
    // Example: "5553:1.2.3.4:53"  → listen="5553", target="1.2.3.4:53"
    // Example: "127.0.0.1:5553:1.2.3.4:53" → listen="127.0.0.1:5553", target="1.2.3.4:53"

    // We need at least two ':' for this to work
    let colons: Vec<usize> = spec.match_indices(':').map(|(i, _)| i).collect();
    if colons.len() < 2 {
        return Err(format!("cannot parse mapping '{}': expected [listen:]:[target]:port", spec));
    }
    // target host:port starts at the second-to-last ':'
    let split_at = colons[colons.len() - 2];
    Ok((&spec[..split_at], &spec[split_at + 1..]))
}

/// Parse `host:port` or `[ipv6]:port`; allow bare port (no host).
fn parse_addr_with_default_host(s: &str, default_host: &str) -> Result<SocketAddr, String> {
    if s.is_empty() {
        return Err("empty address".to_string());
    }
    // If it looks like just a port number
    if s.chars().all(|c| c.is_ascii_digit()) {
        let port: u16 = s.parse().map_err(|e| format!("port: {}", e))?;
        return format!("{}:{}", default_host, port)
            .parse()
            .map_err(|e: std::net::AddrParseError| e.to_string());
    }
    // Otherwise parse normally
    parse_addr_strict(s)
}

fn parse_addr_strict(s: &str) -> Result<SocketAddr, String> {
    s.parse::<SocketAddr>()
        .map_err(|e| format!("invalid address '{}': {}", s, e))
}

// ── Generate a skeleton config ────────────────────────────────────────────────

/// Returns a TOML string with a freshly-generated keypair and documented fields.
pub fn generate_config_text() -> String {
    // Use yggdrasil's generator for the main section, then append yggstack section
    let ygg_text = yggdrasil::config::Config::generate_config_text();

    // Append yggstack section after setting if_name = "none"
    // (yggstack does not use a TUN adapter)
    let ygg_text = ygg_text.replace(
        "\nif_name = \"auto\"",
        "\nif_name = \"none\" # yggstack does not use a TUN adapter",
    );

    format!(
        "{}\n\
# ──────────────────────────────────────────────────────────────────────────────\n\
# yggstack settings\n\
# ──────────────────────────────────────────────────────────────────────────────\n\
[yggstack]\n\
# SOCKS5 listen address (TCP socket or UNIX socket path)\n\
# socks = \"127.0.0.1:1080\"\n\
\n\
# DNS name-server for SOCKS hostname resolution\n\
# nameserver = \"[314:e1b2::53]:53\"\n\
\n\
# Local TCP forwardings (like ssh -L)\n\
# local_tcp = [\"127.0.0.1:8080:[ygg-addr]:80\"]\n\
\n\
# Local UDP forwardings (like ssh -L but for UDP)\n\
# local_udp = [\"127.0.0.1:5553:[ygg-addr]:53\"]\n\
\n\
# Remote TCP forwardings (expose local service to yggdrasil, like ssh -R)\n\
# remote_tcp = [\"80:127.0.0.1:8080\"]\n\
\n\
# Remote UDP forwardings\n\
# remote_udp = [\"53:127.0.0.1:53\"]\n",
        ygg_text
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_local_ipv6_target() {
        let (listen, target) = parse_local_mapping("127.0.0.1:5553:[308:62:45:62::]:53").unwrap();
        assert_eq!(listen.port(), 5553);
        assert_eq!(target.port(), 53);
        assert!(target.is_ipv6());
    }

    #[test]
    fn test_parse_local_bare_port() {
        let (listen, target) = parse_local_mapping("5553:[308:62:45:62::]:53").unwrap();
        assert_eq!(listen.port(), 5553);
        assert_eq!(target.port(), 53);
    }
}
