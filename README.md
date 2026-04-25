# yggstack-ng

A Rust rewrite of [yggstack](https://github.com/yggdrasil-network/yggstack) — a userspace
[Yggdrasil Network](https://yggdrasil-network.github.io/) node with SOCKS5 proxy and TCP/UDP
port-forwarding, built on [Yggdrasil-ng](https://github.com/Revertron/Yggdrasil-ng).

No TUN device or root access required.

## Features

- **SOCKS5 proxy** — route any TCP application through the Yggdrasil mesh
- **DNS resolution** — resolve `.ygg` hostnames through a configurable Yggdrasil DNS server
- **NAT64/DNS64 support** — reach regular IPv4 internet through a NAT64 gateway
- **TCP port forwarding** — expose a local port that tunnels to any Yggdrasil address
- **UDP port forwarding** — same for UDP (DNS, game servers, etc.)
- **No TUN / no root** — uses a userspace TCP/IP stack ([smoltcp](https://github.com/smoltcp-rs/smoltcp))
- **Cross-platform** — Linux, macOS, Windows

## Building

Requires Rust 1.75+.

```sh
cargo build --release -p yggstack
```

The binary is at `target/release/yggstack`.

## Configuration

Generate a new config (TOML format):

```sh
yggstack --genconf > yggstack.toml
```

Minimal example `yggstack.toml`:

```toml
private_key = "<64-byte hex key>"
peers = ["tcp://peer.example.com:1514"]
listen = []
admin_listen = "none"
if_name = "none"
if_mtu = 65535
```

## Usage

```
yggstack [options]

  --useconffile FILE   Read config from FILE (TOML)
  --useconf            Read config from stdin
  --genconf            Print a new random config to stdout
  --address            Print your Yggdrasil IPv6 address
  --subnet             Print your Yggdrasil IPv6 subnet
  --publickey          Print your public key
  --version            Print the build version

  --socks ADDR         Start SOCKS5 proxy on ADDR (e.g. :1080)
  --nameserver ADDR    Yggdrasil DNS server for SOCKS hostname resolution
                       (e.g. [200:peer::dns:addr]:53)

  --local-tcp SPEC     Forward local TCP port to Yggdrasil
                       e.g. 8080:[200:peer::addr]:80
  --local-udp SPEC     Forward local UDP port to Yggdrasil
                       e.g. 553:[200:peer::addr]:53

  --remote-tcp SPEC    Expose Yggdrasil TCP port locally
                       e.g. 22 or 2022:22
  --remote-udp SPEC    Expose Yggdrasil UDP port locally

  --loglevel LEVEL     Log level: error, warn, info, debug, trace (default: info)
  --logto PATH         Log destination: stdout or file path (default: stdout)
```

## Examples

### SOCKS5 proxy to Yggdrasil-only sites

```sh
yggstack --useconffile yggstack.toml \
         --nameserver "[200:peer::dns:addr]:53" \
         --socks :1080

curl --socks5-hostname localhost:1080 http://example.ygg
```

### SOCKS5 proxy to the regular internet via NAT64

Requires a NAT64/DNS64 gateway reachable over Yggdrasil:

```sh
yggstack --useconffile yggstack.toml \
         --nameserver "[200:nat64::dns64:addr]:53" \
         --socks :1080

curl --socks5-hostname localhost:1080 https://example.com
```

### TCP port forward

```sh
yggstack --useconffile yggstack.toml \
         --local-tcp "8080:[200:peer::addr]:80"

curl http://localhost:8080
```

### UDP port forward (DNS)

```sh
yggstack --useconffile yggstack.toml \
         --local-udp "5553:[200:peer::addr]:53"

dig AAAA hostname.ygg @127.0.0.1 -p 5553
```

## Architecture

```
Application ──► SOCKS5 / TCP/UDP listener
                        │
                        ▼
               smoltcp userspace stack
               (IPv6 + TCP + UDP, fragment reassembly)
                        │
                        ▼
              Yggdrasil-ng core (Rust)
              end-to-end encrypted mesh
                        │
                        ▼
              Peer TCP connection(s)
```

## Differences from Go yggstack

| Feature | Go yggstack | yggstack-ng (this) |
|---|---|---|
| Language | Go | Rust |
| Config format | HJSON | TOML |
| IPv6 fragment reassembly | via OS | userspace (smoltcp) |
| TUN required | No | No |

## License

MPL-2.0 — same as the original yggstack.
