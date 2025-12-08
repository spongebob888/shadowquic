# ![image](./logo.svg)

 A 0-RTT QUIC Proxy with SNI camouflage 

 - UDP Friendly with minimum header
 - Full Cone
 - QUIC based 0-RTT
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
### Server
```bash
$ shadowquic -c server.yaml
```

Example config [server.yaml](./shadowquic/config_examples/server.yaml)

Configuration detail can be found in [Documentation](https://spongebob888.github.io/shadowquic/shadowquic/config/struct.Config.html)
## Other Clients
- [clash-rs](https://github.com/Watfaq/clash-rs)
- [husi](https://github.com/xchacha20-poly1305/husi)
- nekobox: [usage](./document/clients/windows.md)
- v2rayN: [usage](./document/clients/windows.md)

## Other Servers
- [docker](https://github.com/spongebob888/shadowquic/pkgs/container/shadowquic): example [coompose file](./shadowquic/config_examples/compose.yaml)
## Protocol
[PROTOCOL](./PROTOCOL.pdf)

## Acknowledgement
 * [JLS](https://github.com/JimmyHuang454/JLS)
 * [TUIC Protocol](https://github.com/tuic-protocol/tuic)
 * [TUIC Itsusinn fork](https://github.com/Itsusinn/tuic)
 * [leaf](https://github.com/eycorsican/leaf)
 * [clash-rs](https://github.com/Watfaq/clash-rs)

