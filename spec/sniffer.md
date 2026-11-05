# Sniffer

Sniffer can sniff the hostname of protocols and expose it to router for routing and must be working with router
(plugin)

sniffer is gated by sniffer feature gate.

It supports plain http, TLS, QUIC.
- For http, use Host as the sniffing target
- For QUIC, only sniffer the first datagram and use it as sniffing result of the proxyrequest. You should
first check the header to fast pass nonquic packet and avoid decrypting every udp packet.

TCP sniffer and UDP sniffer should be seperated and they can optionally be triggered only on certain ports.

The exposed context is RouterContext.UserContext.Sniffer and exposed to lua as ctx.sniffer.domain and
ctx.sniffer.type and ctx.sniffer.version. Type is http/TLS/QUIC. Domain is host or sni. version is tls/quic/http
version.

The sniffer config is global under router.sniffer
```yaml
router:
  sniffer:
    tcp:
      - ports: [80]
        protocols: [http]
      - ports: [443]
        protocols: [tls]
    udp:
      - ports: [] # empty ports sniffing any ports
        protocols: [quic]
    timeout: xx
    other fields needed
```