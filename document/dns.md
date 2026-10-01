# DNS

Build with `cargo build --features dns-server`. DNS is controlled by the `dns-server` feature. Lua helpers require the `plugin` feature (enabled in default builds).
DNS over TLS needs `ring` (default) or `aws-lc-rs`.

Each DNS inbound listens for ordinary DNS over **both UDP and TCP** on
`bind-addr`. Its type selects the resolution method:

| Type | Required fields beyond `tag`, `bind-addr` | Resolution |
| --- | --- | --- |
| `dns-udp` | `upstream` | UDP, with TCP retry for truncated replies |
| `dns-tcp` | `upstream` | Length-prefixed DNS over TCP |
| `dns-tls` | `upstream`, `server-name` | DNS over TLS with rustls-jls certificate and hostname verification |
| `dns-system` | None | Tokio system lookup for A/AAAA |
| `dns-fakeip` | None | Stable synthetic A/AAAA answers |

Each inbound type has its own configuration struct and rejects fields belonging
to other DNS types. UDP/TCP/TLS require `upstream`; TLS also requires
`server-name`. System and fake-IP configurations contain only `tag` and
`bind-addr`.

`upstream` is a literal socket address, such as `1.1.1.1:853` or
`[2606:4700:4700::1111]:853`, so bootstrap resolution cannot recurse into DNS.
`server-name` is the TLS identity; public WebPKI roots are used. DNS uses
`rustls-jls` and `tokio-rustls-jls` with JLS explicitly disabled, so upstream
connections use standard TLS and verify certificates. Local listeners
use plain DNS even for `dns-tls`; that type encrypts the upstream connection.

UDP, TCP, and TLS upstream traffic becomes a `ProxyRequest` tagged with the DNS
inbound's tag. The usual router and outbounds carry it, including through a
SOCKS or QUIC proxy. System and fake-IP services answer locally. Queries have a
five-second timeout; listener concurrency and queues are bounded. Failed valid
queries receive SERVFAIL. System lookup cannot distinguish NXDOMAIN from other
OS errors, so those also produce SERVFAIL. System DNS returns NOTIMP for record
types other than A/AAAA; fake-IP DNS returns an empty successful answer for them.
Both local services use a 60-second answer TTL.

Each DNS inbound automatically registers an outbound under the same tag. Route
intercepted UDP DNS sessions directly to that tag (for example, `return "fake"`).
No explicit DNS outbound configuration is needed. DNS inbound tags must not
collide with explicit outbound tags. `router.default-outbound` can also select a
DNS inbound tag. Without an explicit default, the first configured outbound is
used; if there are no explicit outbounds, the first DNS inbound is used. Local
system/fake-IP services can therefore run without an `outbounds` section.
Replies retain the original DNS destination as their source address.
Route the DNS inbound's own upstream traffic to a transport outbound, before
any UDP-port-53 hijacking rule, to avoid recursive DNS interception.

`direct`, `socks`, `shadowquic`, and `sunnyquic` outbounds accept an optional
`dns: <inbound-tag>`. This resolves **requested destination domains after
routing**, including each UDP datagram, using that DNS service. It preserves
ports and original UDP reply addresses. Direct outbounds retain their
`dns-strategy`; proxy outbounds prefer IPv4. Omit `dns` to retain existing
resolution behavior (including remote destination resolution by proxy
outbounds). This option does not change bootstrap resolution of a proxy
server's own `addr`. Fake-IP services cannot be selected for outbound
destination resolution.

The process-wide positive DNS cache holds up to 4,096 responses, partitions
responses by resolver instance, adjusts TTLs on hits, and expires entries at the
shortest record TTL. Zero-TTL, failed, empty, and truncated answers are not
cached. Lua routing scripts can call:

```lua
local addresses = lookup_cache("example.com") -- array of IP strings; empty on miss
local domain = reverse_lookup("203.0.113.1")  -- most recently cached name, or nil
```

These functions only inspect unexpired cached answers; they never perform
network I/O. Reverse lookup follows cached question/CNAME answers, rather than
issuing a PTR query. Multiple names may share an address.

Only one `dns-fakeip` inbound is allowed per configuration. It allocates from
`198.18.0.0/15` and `fd00::/96`, with at most 131,070 domain mappings. Mappings
are stable and are never recycled while the manager runs; exhaustion fails
instead of reassigning an active IP. They are not persisted across restarts.
With Linux TPROXY, fake destinations are restored to domain names **before
routing**, and every UDP packet is translated as well. UDP replies retain the
fake source IP expected by the application. Unmapped addresses in these pools
are rejected. Configure your OS routes and TPROXY interception for these ranges
when using this mode. Fake addresses must not be used as real outbound targets.

See [`config_examples/dns.yaml`](../shadowquic/config_examples/dns.yaml) for a
complete configuration with TLS resolution and DNS hijacking.

The live AliDNS integration test sends a query through the local DNS listener
and routes it over certificate-verified TLS to `223.5.5.5:853`, using
`dns.alidns.com` as the server name. It is ignored by default because it requires
Internet access. With `plugin` enabled, the same test target also checks two DNS
inbounds with Lua routing: `dns-udp → dns-tls → direct`. It verifies that the UDP
inbound's original upstream receives no packets. Run both tests explicitly with:

```sh
cargo test --release -p shadowquic --features dns-server --test dns_tls_alidns -- --ignored --nocapture
```
