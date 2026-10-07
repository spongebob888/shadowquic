# DNS

Build with `cargo build --features dns-server`. DNS is controlled by the `dns-server` feature. Lua helpers require the `plugin` feature (enabled in default builds).
DNS over TLS needs `ring` (default) or `aws-lc-rs`.

DNS services are configured in the top-level `dns` list:

```yaml
dns:
  - tag: resolver
    type: dns-tls
    bind-addr: 127.0.0.1:1053
    upstream: 1.1.1.1:853
    server-name: cloudflare-dns.com
```

Each DNS service listens for ordinary DNS over **both UDP and TCP** on
`bind-addr`. Its type selects the resolution method:

| Type | Required fields beyond `tag`, `bind-addr` | Resolution |
| --- | --- | --- |
| `dns-udp` | `upstream` | UDP, with TCP retry for truncated replies |
| `dns-tcp` | `upstream` | Length-prefixed DNS over TCP |
| `dns-tls` | `upstream`, `server-name` | DNS over TLS with rustls-jls certificate and hostname verification |
| `dns-system` | None | Tokio system lookup for A/AAAA |
| `dns-fakeip` | None | Stable synthetic A/AAAA answers |

Each service type has its own configuration struct and rejects fields belonging
to other DNS types. UDP/TCP/TLS require `upstream`; TLS also requires
`server-name`. All types except `dns-fakeip` accept `bypass-cache` (default:
`false`). Set it to `true` to skip shared DNS cache reads and writes for that
service. This does not disable caching performed by the operating system or
upstream DNS server. Fake-IP configurations contain only `tag` and `bind-addr`.

`upstream` is a literal socket address, such as `1.1.1.1:853` or
`[2606:4700:4700::1111]:853`, so bootstrap resolution cannot recurse into DNS.
`server-name` is the TLS identity; public WebPKI roots are used. DNS uses
`rustls-jls` and `tokio-rustls-jls` with JLS explicitly disabled, so upstream
connections use standard TLS and verify certificates. Local listeners
use plain DNS even for `dns-tls`; that type encrypts the upstream connection.

UDP, TCP, and TLS upstream traffic becomes a `ProxyRequest` tagged with the DNS
service's tag. The usual router and outbounds carry it, including through a
SOCKS or QUIC proxy. System and fake-IP services answer locally. Queries have a
five-second timeout; listener concurrency and queues are bounded. Failed valid
queries receive SERVFAIL. System lookup cannot distinguish NXDOMAIN from other
OS errors, so those also produce SERVFAIL. System DNS returns NOTIMP for record
types other than A/AAAA; fake-IP DNS returns an empty successful answer for them.
Both local services use a 60-second answer TTL.

Lua routing scripts can inspect upstream questions through `ctx.dns_query`,
an array of tables containing `name` and numeric `record_type` (1 for A, 28 for
AAAA). It is empty when the request has no DNS metadata. Changes to these
tables do not modify the request or DNS packet. For example:

```lua
return function(ctx)
    for _, question in ipairs(ctx.dns_query) do
        if question.name == "example.com" and question.record_type == 1 then
            return "direct"
        end
    end
    return "proxy"
end
```

The returned tags must refer to configured transport outbounds.

Each DNS service automatically registers an outbound under the same tag. Route
intercepted UDP DNS sessions directly to that tag (for example, `return "fake"`).
No explicit DNS outbound configuration is needed. DNS service tags must not
collide with explicit outbound tags. `router.default-outbound` can also select a
DNS service tag. Without an explicit default, the first configured outbound is
used; if there are no explicit outbounds, the first DNS service is used. Local
system/fake-IP services can therefore run without an `outbounds` section.
Replies retain the original DNS destination as their source address.
Route the DNS service's own upstream traffic to a transport outbound, before
any UDP-port-53 hijacking rule, to avoid recursive DNS interception.

`direct`, `socks`, `shadowquic`, and `sunnyquic` outbounds accept an optional
`dns: <service-tag>`. This resolves **requested destination domains after
routing**, including each UDP datagram, using that DNS service. It preserves
ports and original UDP reply addresses. Direct outbounds retain their
`dns-strategy`; proxy outbounds prefer IPv4. Omit `dns` to retain existing
resolution behavior (including remote destination resolution by proxy
outbounds). This option does not change bootstrap resolution of a proxy
server's own `addr`. Fake-IP services cannot be selected for outbound
destination resolution.

The positive DNS cache is owned by the manager and holds up to 4,096
responses shared by non-fake-IP resolvers in that manager unless
`bypass-cache: true` is set on a service. Bypassing leaves existing shared
entries untouched. Fake-IP services
bypass cache reads and writes and maintain their own stable address mappings.
Entries are keyed by the query without its transaction ID, so non-fake-IP
resolvers can reuse matching responses from each other. The cache
adjusts TTLs on hits and expires entries at the
shortest record TTL. Zero-TTL, failed, empty, and truncated answers are not
cached. Lua routing scripts can call:

```lua
local addresses = lookup_cache("example.com") -- array of IP strings; empty on miss
local domain = reverse_lookup_cache("203.0.113.1")  -- most recently cached name, or nil
```

These functions only inspect unexpired cached answers; they never perform
network I/O. Reverse lookup searches both cached A/AAAA answers and cached PTR
queries for the IP address, following CNAME aliases in either case. It returns a
name from the most recently cached matching response; for multiple PTR targets,
it returns the first. It does not issue a new PTR query. Multiple names may
share an address.

With `dns-server` and `plugin` enabled, routing functions can also call
`lookup(dns_tag, domain)` and `reverse_lookup(dns_tag, ip)`. They return arrays
of IP strings (A and AAAA) and hostname strings (PTR), respectively. Both use
the selected configured DNS service and its shared cache. Unknown resolver
tags, invalid inputs, and failed lookups raise Lua errors; use `pcall` to
handle them in a script. The built-in `default-system` resolver supports
forward lookups; PTR lookups require a UDP, TCP, or TLS resolver.

These calls suspend the current routing function while waiting for DNS.
Other requests can route during that wait. Call them inside the returned
routing function, rather than during script initialization. DNS upstream
requests also pass through the router, so route them to a transport outbound
before making lookups:

```lua
return function(ctx)
    if #ctx.dns_query > 0 then return "direct" end
    if ctx.dst_domain then
        local ips = lookup("resolver", ctx.dst_domain)
        local ok, names = pcall(reverse_lookup, "resolver", ips[1])
        if ok then print(names[1]) end
    end
    return "proxy"
end
```

Use configured transport outbound tags in place of `direct` and `proxy`.
Routing functions share Lua state and may interleave at these calls. Reloads
apply to new requests; suspended routing functions finish on their original
script runtime.

Rust callers can use `DnsService::reverse_lookup(ip).await` to issue a PTR query
through the selected service. It supports IPv4 (`in-addr.arpa`) and IPv6
(`ip6.arpa`), follows CNAME aliases, and returns distinct hostnames as
`Vec<String>`. Responses use the normal DNS cache and routing path; failed or
empty lookups return an error. System and fake-IP services do not provide PTR
records, so use a UDP/TCP/TLS service for this method.

Only one `dns-fakeip` service is allowed per configuration. It allocates from
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
services with Lua routing: `dns-udp → dns-tls → direct`. It verifies that the UDP
service's original upstream receives no packets. Run both tests explicitly with:

```sh
cargo test --release -p shadowquic --features dns-server --test dns_tls_alidns -- --ignored --nocapture
```
