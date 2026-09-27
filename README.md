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

### Multiple instances

Repeat `-c` (or `--config`) to run one instance per configuration in the same process:

```bash
$ shadowquic -c config1.yaml -c config2.yaml
```

The explicit `run` subcommand also supports multiple configs. Without `-c`,
ShadowQUIC loads `config.yaml`. Each instance uses its own inbound and outbound;
configure distinct listener addresses or ports and separate user/statistics files
where applicable. Relative paths are resolved from the working directory.
Logging uses the most verbose explicitly configured `log-level`, ignoring omitted
or null values. If none is specified, logging defaults to `info`. Ctrl-C or SIGTERM
shuts down all instances. A fatal startup or runtime error stops the process.
The `api` subcommand accepts exactly one config.

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

Configuration detail can be found in [Documentation](https://spongebob888.github.io/shadowquic/configuration/)
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
