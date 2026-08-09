# Configuration explained in plain words

ShadowQUIC's config is a **YAML file** with three sections:

```yaml
inbound:    # how I accept traffic
outbound:   # where traffic goes
log-level:  # how verbose the logs are (optional)
```

No need to memorize anything — the tables below translate every field into
human language.

## Server config (server.yaml) walkthrough

```yaml
inbound:
  type: shadowquic        # inbound protocol: how clients connect to me
  bind-addr: "0.0.0.0:1443"   # which port to listen on (0.0.0.0 = all interfaces)
  users:                  # allowed username/password list
    - username: "admin"
      password: "hello"
  jls-upstream:           # ★ camouflage settings
    addr: "cloudflare.com:443"  # disguise as visiting this domain (changeable)
    rate-limit: 1000000   # optional: forward rate limit (bps), unlimited by default
  server-name: "cloudflare.com"  # optional: server name used to check the client
  alpn: ["h3"]            # TLS application protocols; keep the default
  congestion-control: bbr # congestion control; bbr is best on lossy networks
  zero-rtt: true          # enable 0-RTT for faster connections
  initial-mtu: 1300       # initial packet size; don't change casually
  min-mtu: 1290           # minimum packet size; don't change casually
outbound:
  type: direct            # server connects to targets directly
  dns-strategy: prefer-ipv4  # DNS preference (see table below)
```

### Field-by-field plain talk

| Field | Plain words |
|-------|-------------|
| `type` | "What kind is this section". Both inbound and outbound need it |
| `bind-addr` | The "address:port" to listen on. `0.0.0.0` means all interfaces |
| `users` | Whitelist. Only these usernames/passwords can connect |
| `jls-upstream.addr` | Disguise traffic as visiting this "domain:port". A real, well-known site works best |
| `jls-upstream.rate-limit` | Max speed (bit/s) when forwarding disguise traffic, so the camouflage server isn't abused |
| `server-name` | Optional: server name used to check the client. If empty, parsed from `jls-upstream` automatically |
| `alpn` | Protocols announced during the TLS handshake. Ends must overlap; default `["h3"]` is fine |
| `congestion-control` | How to send data on a bad network. `bbr` survives packet loss; `cubic`/`new-reno` are classics |
| `zero-rtt` | Skip the handshake wait on later connections. Keep it on for better UX |
| `gso` | Batch-send packets when the OS supports it, saving CPU. Keep it on |
| `mtu-discovery` | Auto-detect the largest packet the network can carry. Can disable on unstable UDP networks |
| `min-mtu` | Minimum packet size; must be smaller than `initial-mtu`, don't change casually |
| `blackhole-detection` | Keep it off (default) on lossy networks to avoid repeated MTU resets |

> **How does JLS camouflage work?** The server treats the domain in
> `jls-upstream.addr` as its "stand-in". When a client connects, the TLS
> handshake's SNI field carries that domain, so from the outside it looks like
> a normal visit to `cloudflare.com`. The `users` usernames/passwords double
> as the JLS authentication credentials — the client must present an account
> from the server's whitelist to pass the handshake.

## Client config (client.yaml) walkthrough

```yaml
inbound:
  type: socks             # start a local SOCKS5 proxy
  bind-addr: "127.0.0.1:1089"   # local listen port (software usually uses 1080/1089)
  users: []               # optional: password for the local proxy (empty = none needed)
outbound:
  type: shadowquic        # outbound protocol: send traffic to the remote server
  addr: "1.2.3.4:1443"    # server address:port (IP or domain)
  username: "admin"       # must match server.yaml
  password: "hello"
  server-name: "cloudflare.com"  # ★ must match the server's jls-upstream domain
  alpn: ["h3"]
  zero-rtt: true
  congestion-control: bbr
  over-stream: false      # UDP over datagram (false) or over stream (true)
  keep-alive-interval: 0  # keep-alive interval (ms); 0 = off
```

### Extra client fields

| Field | Plain words |
|-------|-------------|
| `addr` | The remote server's "address:port". IP or domain both work |
| `server-name` | Used to verify the server's identity. **Must match the server's `jls-upstream` domain exactly** |
| `over-stream` | Which channel UDP uses. `false` = datagram (low latency, no retransmission); `true` = QUIC stream (retransmits, not for heavy UDP) |
| `keep-alive-interval` | How often to send a heartbeat (ms). 0 = off (should be under 30000 ms idle timeout) |
| `bind-interface` | Force outgoing packets out a specific NIC (e.g. `eth0`, `127.0.0.1`). Recommended with TUN-based proxies like sing-box/mihomo |
| `protect-path` | Android only: Unix socket path used to protect the underlying socket via the VPN |
| `cipher-suite-preference` | Optional TLS 1.3 cipher suite preference. Leave empty to use the default order |

## Inbound choices: `inbound.type`

| type | Plain words |
|------|-------------|
| `socks` | Open a local SOCKS5 port; apps throw traffic at it |
| `mixed` | One port that auto-detects SOCKS5 and HTTP proxy (needs `mixed` feature) |
| `shadowquic` | For remote servers: clients connect with the `shadowquic` protocol (JLS camouflage, no certs) |
| `sunnyquic` | Another QUIC protocol (needs real certs, supports multipath) for networks with certs |
| `tproxy` | Linux transparent proxy: no app changes needed, routing rules redirect traffic (Linux only, needs `tproxy` feature) |

## Outbound choices: `outbound.type`

| type | Plain words |
|------|-------------|
| `shadowquic` | Encrypt traffic and send it to a remote shadowquic server |
| `sunnyquic` | Send to a remote sunnyquic server |
| `socks` | Forward to another SOCKS5 proxy (proxy chaining) |
| `direct` | Connect to the target directly (used on servers) |

> `mixed` and `tproxy` features are on by default in official builds; you
> usually don't need to worry about them.

## Congestion control algorithms

`congestion-control` supports:

| Value | Plain words |
|-------|-------------|
| `bbr` | Default. Keeps throughput under loss; first choice on bad networks |
| `cubic` / `new-reno` | Classic algorithms; fine on good networks |
| `brutal` | Aggressively grabs bandwidth (like a "speed-up" hack). **Important:** `brutal` takes an object, not a string, and it grabs **upload** bandwidth; to cap download, set it on the peer (e.g. on the server side for a client) |

Example:

```yaml
congestion-control:
  brutal:
    bandwidth: 10000000   # upload bandwidth in bps
```

## The one-paragraph summary

- A config file = `inbound` (accept traffic) + `outbound` (send traffic)
- **Server**: `inbound` = `shadowquic`, `outbound` = `direct`; don't forget
  to open the UDP port in the firewall
- **Client**: `inbound` = `socks`, `outbound` = `shadowquic`; remember
  `server-name` must match the server's disguise domain
- When unsure, keep the defaults — get it working first, tune later

The complete field list (with defaults and required flags) lives in the
auto-generated **Configuration** section in the navigation bar.

## Next

Check the [FAQ](faq.md), or go back to [Quick Start](quickstart.md).
