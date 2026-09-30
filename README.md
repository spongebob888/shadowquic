# ![image](./logo.svg)

 A 0-RTT QUIC Proxy with SNI camouflage 

 - UDP Friendly with minimum header
 - Full Cone
 - QUIC based 0-RTT
 - [User Management](./document/api.md)
 - SNI camouflage with any domain (powered by [JLS](https://github.com/JimmyHuang454/JLS))
    - Anti-hijack
    - Resisting active detection
    - Free of certificates

## Usage
### Client
```bash
$ shadowquic -c client.yaml
```

Example config: [client.yaml](./shadowquic/config_examples/client.yaml)

### [Clash-rs](https://github.com/Watfaq/clash-rs)
```yaml
# config.yaml
{
  name: "node_name",
  type: shadowquic,
  server: "1.1.1.1",
  port: 1443,
  username: "my_name",
  password: "my_password",
  server-name: "cloudflare.com"
}
```

### Server
#### Installation Script (Linux)
```bash
$ curl -L https://raw.githubusercontent.com/spongebob888/shadowquic/main/scripts/linux_install.sh | bash
```
This script will:
- Install `shadowquic` to `/usr/local/bin/`
- Generate random credentials and config at `/etc/shadowquic/server.yaml`
- Setup and start `shadowquic` systemd service
```bash
$ systemctl start shadowquic.service
$ systemctl stop shadowquic.service
```

#### Manual Usage
```bash
$ shadowquic -c server.yaml
```

Example config [server.yaml](./shadowquic/config_examples/server.yaml)

Configuration detail can be found in [Documentation](https://spongebob888.github.io/shadowquic/main/configuration/)

Use `inbounds` and `outbounds` lists. Every endpoint requires a nonempty `tag`,
unique within its list. All inbounds run concurrently and route through the
outbound named by `router.default-outbound`; if omitted, the first outbound in the list
is used. For example:

```yaml
inbounds:
- type: socks
  tag: local-socks
  bind-addr: "127.0.0.1:1080"
- type: socks
  tag: second-socks
  bind-addr: "127.0.0.1:1081"
outbounds:
- type: direct
  tag: direct
- type: socks
  tag: upstream
  addr: "127.0.0.1:1082"
router:
  default-outbound: direct
```

Both listeners above use `direct`. Additional outbounds are available by tag;
API commands select one with `api --outbound TAG`. Existing singular
`inbound`/`outbound` configs continue to work unchanged. Each object becomes a
single-entry list; omitted tags default to `inbound` and `outbound`, respectively.
Explicit tags are preserved. List entries still require tags, and specifying both
the singular and plural key for the same direction is an error.

With the `plugin` feature, configure request routing with either inline Lua source
or a script file:

```yaml
router:
  src: |
    return function(ctx) return "direct" end
```

```yaml
router:
  path: router.lua
```

Set only one of `src` and `path`. Without a script, requests use
`router.default-outbound`, or the first outbound if it is omitted. Selecting a
default outbound does not require the `plugin` feature. File paths resolve from
the process working directory.
Changes reload automatically for subsequent requests.
Read or script-loading errors are logged and the last working
router stays active. A successful reload resets Lua script state; existing
connections are unaffected. Inline `src` scripts are not watched.

The script returns a function that receives the request context and returns a
configured outbound tag, or `nil, error_message` to reject the request. See the
[router configuration reference](https://spongebob888.github.io/shadowquic/main/configuration/router/)
for context fields and destination rewriting.

## Other Clients
- [husi](https://github.com/xchacha20-poly1305/husi)
- nekobox: [usage](./document/clients/windows.md)
- v2rayN: [usage](./document/clients/windows.md)
- [QuicProxy](https://github.com/RealBikiniBottom/QuicProxy): GUI and core
- mihomo

## Other Servers
- [docker](https://github.com/spongebob888/shadowquic/pkgs/container/shadowquic): example [compose file](./shadowquic/config_examples/compose.yaml)
- [QuicProxy](https://github.com/RealBikiniBottom/QuicProxy): GUI and core
- mihomo
## Protocol
[PROTOCOL](./PROTOCOL.pdf)

## Interop
[ShadowQUIC Interop](https://spongebob888.github.io/shadowquic_interop)

## Acknowledgement
 * [JLS](https://github.com/JimmyHuang454/JLS)
 * [TUIC Protocol](https://github.com/tuic-protocol/tuic)
 * [TUIC Itsusinn fork](https://github.com/Itsusinn/tuic)
 * [leaf](https://github.com/eycorsican/leaf)
 * [clash-rs](https://github.com/Watfaq/clash-rs)
