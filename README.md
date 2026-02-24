# yggstack-ng

**yggstack-ng** is a pure-Rust implementation of [yggstack](https://github.com/yggdrasil-network/yggstack) — it runs a headless [Yggdrasil](https://yggdrasil-network.github.io) node (no TUN/TAP adapter) and exposes your connectivity as:

- a **SOCKS5 proxy** (reach any Yggdrasil node by IPv6 address or hostname)
- **local port forwards** — tunnel a remote Yggdrasil service to a local port (`ssh -L` style)
- **remote port forwards** — expose a local service on your Yggdrasil address (`ssh -R` style)

---

## Installation

### Pre-built binaries

Download the latest binary for your platform from the [Releases](../../releases) page.

| Platform | File |
|---|---|
| Linux x86_64 | `yggstack-linux-amd64` |
| Linux ARM64 | `yggstack-linux-arm64` |
| macOS x86_64 | `yggstack-macos-amd64` |
| macOS ARM64 | `yggstack-macos-arm64` |
| Windows x86_64 | `yggstack-windows-amd64.exe` |
| Windows ARM64 | `yggstack-windows-arm64.exe` |

### Build from source

```bash
git clone https://github.com/YOUR_USERNAME/yggstack-ng
cd yggstack-ng
cargo build --release
# binary at: target/release/yggstack
```

---

## Quick start

### 1. Generate a config file

```bash
yggstack --genconf yggstack.toml
```

This writes a fresh config with a newly generated keypair. Edit the `peers` list to connect to the network.

### 2. Run

```bash
yggstack --config yggstack.toml
```

Or use **autoconf** for a one-shot ephemeral node (random key, no config file needed):

```bash
yggstack --autoconf --socks 127.0.0.1:1080
```

---

## Config file

`yggstack.toml` is split into two sections: `[yggdrasil]` (core node settings) and `[yggstack]` (proxy/forward settings).

```toml
# ── Yggdrasil node settings ────────────────────────────────────────────────────

# Your persistent ed25519 private key (64-byte hex).
# Generate with: yggstack --genconf
private_key = "abc123...def456"

if_name = "none"  # yggstack never creates a TUN adapter

# Peers to connect to on startup.
peers = [
  "tls://[2a00:1450:400f:80c::200e]:443",
  "tcp://example.com:9001",
]

# Optional: allow specific peers to connect to you
# listen_addresses = ["tcp://0.0.0.0:9001"]

# Optional: enable multicast peer discovery on LAN
# [multicast]
# enabled = true

# ── yggstack settings ──────────────────────────────────────────────────────────

[yggstack]

# SOCKS5 proxy listen address
socks = "127.0.0.1:1080"

# DNS resolver used for SOCKS5 hostname resolution.
# Point this at a DNS-over-Yggdrasil server.
nameserver = "[314:e1b2::53]:53"

# Local TCP port forwards (like ssh -L):
#   [bind_addr:]bind_port:[ygg_addr]:ygg_port
local_tcp = [
  "127.0.0.1:8080:[300:1234:abcd::1]:80",
  "2222:[300:1234:abcd::1]:22",
]

# Local UDP port forwards:
local_udp = [
  "127.0.0.1:5353:[300:1234:abcd::1]:53",
]

# Remote TCP port forwards (like ssh -R) — expose a local service on your
# Yggdrasil address:
#   ygg_port[:local_addr:local_port]
remote_tcp = [
  "8080:127.0.0.1:8080",   # expose local :8080 as ygg-addr:8080
  "22",                     # expose local :22  as ygg-addr:22  (same port)
]

# Remote UDP port forwards:
remote_udp = [
  "53:127.0.0.1:53",
]
```

---

## CLI reference

```
yggstack [OPTIONS]

Options:
  -c, --config <FILE>          Config file path [default: yggstack.toml]
      --genconf [FILE]         Generate a skeleton config and exit
      --autoconf               Run with ephemeral keys (no config file needed)
      --address                Print this node's Yggdrasil IPv6 address and exit
  -l, --loglevel <LEVEL>       Log level: error/warn/info/debug/trace [default: info]
      --socks <ADDR>           SOCKS5 listen address (overrides config)
      --nameserver <ADDR>      DNS name-server for SOCKS5 hostname resolution
      --local-tcp <MAPPING>    Local TCP forward (repeatable)
      --local-udp <MAPPING>    Local UDP forward (repeatable)
      --remote-tcp <MAPPING>   Remote TCP forward (repeatable)
      --remote-udp <MAPPING>   Remote UDP forward (repeatable)
  -h, --help                   Print help
  -V, --version                Print version
```

---

## Examples

### SOCKS5 proxy

```bash
yggstack --config yggstack.toml --socks 127.0.0.1:1080
```

Then configure your browser or tool to use `127.0.0.1:1080` as a SOCKS5 proxy. You can now connect to any Yggdrasil node directly:

```bash
curl --socks5-hostname 127.0.0.1:1080 http://[300:1234:abcd::1]/
```

### Print your Yggdrasil address

```bash
yggstack --config yggstack.toml --address
# 300:abcd:1234:5678::1
```

### Local TCP port forward — access a remote SSH server

Forward `localhost:2222` → Yggdrasil node `[300:1234:abcd::1]:22`:

```bash
# via CLI flag
yggstack --config yggstack.toml --local-tcp 2222:[300:1234:abcd::1]:22

# or in config:
# local_tcp = ["2222:[300:1234:abcd::1]:22"]
```

```bash
ssh -p 2222 user@127.0.0.1
```

### Local UDP forward — use a remote DNS resolver

```bash
yggstack --config yggstack.toml --local-udp 5353:[300:1234:abcd::1]:53
dig @127.0.0.1 -p 5353 example.com
```

### Remote TCP forward — expose a local web server

Make your `localhost:8080` reachable by other Yggdrasil nodes as `[your-ygg-addr]:8080`:

```bash
yggstack --config yggstack.toml --remote-tcp 8080
```

Or map to a different local port:

```bash
yggstack --config yggstack.toml --remote-tcp 80:127.0.0.1:8080
```

### Ephemeral node (no config file)

Useful for scripting or one-off tunnels. Generates a random key on every run:

```bash
yggstack --autoconf \
  --socks 127.0.0.1:1080 \
  --local-tcp 2222:[300:1234:abcd::1]:22
```

### Private key via environment variable

```bash
export YGGDRASIL_PRIVATE_KEY="<64-byte hex key>"
yggstack --autoconf --socks 127.0.0.1:1080
```

---

## Mapping format

| Type | Format | Example |
|---|---|---|
| Local TCP/UDP | `[bind_addr:]bind_port:[ygg_addr]:ygg_port` | `127.0.0.1:8080:[300:ab::1]:80` |
| Local TCP/UDP | `bind_port:[ygg_addr]:ygg_port` | `8080:[300:ab::1]:80` |
| Remote TCP/UDP | `ygg_port` | `22` (same local port) |
| Remote TCP/UDP | `ygg_port:local_port` | `8080:3000` |
| Remote TCP/UDP | `ygg_port:local_addr:local_port` | `80:127.0.0.1:8080` |

---

## Peers

A list of public Yggdrasil peers is maintained at:  
**https://publicpeers.neilalexander.dev/**

---

## License

MPL-2.0
