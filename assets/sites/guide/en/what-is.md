# What is ShadowQUIC? (in plain words)

## TL;DR

> ShadowQUIC is a **proxy tool**: you run one copy on a server, one on your
> local machine, and route traffic between them through a QUIC tunnel that is
> **fast and hard to block**.

## What does it do?

In plain words: you want to reach some websites from home or work, but the
direct connection is blocked, slow, or heavily interfered with. ShadowQUIC
routes your traffic "the long way around":

```
Your computer → local client → (encrypted QUIC tunnel) → remote server → the website you actually want
```

The remote server visits the site for you and sends the data back. From a
firewall's point of view, you are just "chatting with an ordinary website" —
it cannot tell that you are visiting something else.

## What makes it different from other proxies?

There are lots of proxies out there (SOCKS5, Shadowsocks, V2Ray, Trojan…).
ShadowQUIC's signature tricks:

| Feature | Plain-word explanation |
|---------|------------------------|
| **0-RTT handshake** | Almost no waiting when opening a connection — pages feel snappier |
| **QUIC-based** | Uses the same transport as HTTP/3, stays stable on lossy networks |
| **UDP friendly** | Gaming, voice, video — UDP traffic works too, with low latency |
| **Full Cone** | Strong NAT traversal, two-way communication works behind NAT |
| **SNI camouflage** | Traffic is disguised as visiting a well-known site like `cloudflare.com` |
| **No certificate needed** | Thanks to camouflage, the server does NOT need its own TLS certificate |

> Good to know: it has a sibling protocol, **SunnyQUIC**, which uses real TLS
> certificates and supports multipath aggregation. Camouflage vs multipath is
> a trade-off — see the [FAQ](faq.md).

## Two keywords: Server and Client

One binary, two roles, decided by the config file:

- **Server**: runs on the remote machine, listens on a port, waits for
  clients, and actually fetches websites for you. Config has
  `inbound: type: shadowquic`.
- **Client**: runs locally, opens a local proxy port (e.g. SOCKS5 at
  `127.0.0.1:1089`), encrypts your traffic, and sends it to the server.
  Config has `outbound: type: shadowquic`.

Actually every config file contains **both** sections: `inbound` decides
"how do I accept traffic", `outbound` decides "where does traffic go out".
A server usually has `inbound: shadowquic` + `outbound: direct`. A client
usually has `inbound: socks` + `outbound: shadowquic`.

## It also supports other inbound/outbound types

- **Inbound** (how traffic comes in): `socks` (local SOCKS5),
  `shadowquic`, `sunnyquic`, `mixed` (auto SOCKS + HTTP), `tproxy`
  (transparent proxy on Linux)
- **Outbound** (where traffic goes): `shadowquic`, `sunnyquic`,
  `socks`, `direct`

## Typical use cases

- Personal circumvention (client on your phone/laptop, server on a
  foreign VPS)
- LAN sharing (run it on a router, all devices in the house share it)
- Gaming acceleration (UDP friendly + Full Cone = NAT-friendly)

## Next step

[Quick Start](quickstart.md) → get it running hands-on.
