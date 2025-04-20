![image](./logo.svg)

 A 0-RTT QUIC Proxy with SNI camouflage 

 - UDP Friendly with minimum header
 - Full Cone
 - QUIC based 0-RTT
 - SNI camouflage with any domain (powered by [JLS](https://github.com/JimmyHuang454/JLS))
 - Anti-hijack
 - Resisting active deteingction
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

## Protocol
[PROTOCOL](./PROTOCOL.pdf)

## Acknowlegement
 * [JLS](https://github.com/JimmyHuang454/JLS)
 * [TUIC Protocol](https://github.com/tuic-protocol/tuic)
 * [TUIC Itsusinn fork](https://github.com/Itsusinn/tuic)
 * [leaf](https://github.com/eycorsican/leaf)

