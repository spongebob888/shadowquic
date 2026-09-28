# FAQ

## What's the difference between `shadowquic` and `sunnyquic`?

Two protocols shipped in the same binary; the `type` field picks which one:

| | `shadowquic` | `sunnyquic` |
|---|---|---|
| Camouflage | **SNI camouflage** — traffic looks like visiting a well-known site, no certs needed | Real TLS certificates |
| Certificate | None needed | Server needs `cert-path` / `key-path` |
| Multipath | Single path | Multipath (`max-path-num`, `extra-paths`) — aggregate multiple NICs / dual-stack |
| Use case | Most users, censorship-prone networks (recommended) | You have certs, need multipath / high-reliability |

They share the same QUIC transport and UDP session logic; only the "front door"
differs.

## Can't connect / handshake timeout?

Usually one of these three. Check in order:

1. **UDP port not opened in the firewall.** ShadowQUIC uses UDP, not TCP.
   In your server's firewall / security group, open the `bind-addr` port for
   **UDP** (opening TCP is optional).
2. **`server-name` doesn't match the server.** The client's `server-name`
   must match the domain in the server's `jls-upstream` **exactly** (case,
   port included).
3. **No `alpn` overlap.** Both ends need at least one common protocol in
   `alpn`. Keep the default `["h3"]` unless you know what you're doing.

## Connected but slow / heavy packet loss?

- Set `congestion-control: bbr` on both ends (best at surviving loss).
- On high-loss networks, set `mtu-discovery: false` and raise
  `initial-mtu` manually (e.g. `1400`).
- Disable `blackhole-detection` on lossy networks (it is off by default)
  so the MTU isn't repeatedly reset.

## UDP (gaming / voice) not working?

- Make sure the client has `over-stream: false` (UDP over datagram, lowest
  latency).
- ShadowQUIC supports **Full Cone**, which is strong at NAT traversal, but
  an extremely strict router/ISP NAT can still interfere.
- When proxying HTTP/3 traffic, keep `over-stream: false` and disable
  `blackhole-detection` so HTTP/3's MTU probing isn't broken.

## Do I need a domain and certificate for the server?

No — that's ShadowQUIC's signature feature:

- Set `jls-upstream.addr` to **someone else's** well-known site (e.g.
  `cloudflare.com:443`).
- Your server uses that domain as its "disguised identity"; the client
  recognizes it via `server-name`.
- You never need to obtain a TLS certificate.

> Note: the disguise domain must be **real and reachable**, otherwise it can
> be detected.

## What's special about usernames starting with `admin`?

Accounts whose username starts with `admin` (e.g. `admin`, `admin_bob`,
`admin123`) have **administrator rights** and can manage the server remotely
via the `shadowquic api` subcommand:

```bash
shadowquic api list-users                 # list all users
shadowquic api add-user alice alice-pass  # add or update a user
shadowquic api remove-user bob            # remove a user (drops its connections)
shadowquic api get-stats alice            # traffic stats for one user
shadowquic api get-stats                  # traffic stats for all users
shadowquic api kill-conn alice            # drop all online connections of a user
shadowquic api clear-stats alice          # reset one user's traffic counters
shadowquic api clear-stats                # reset every user's traffic counters
```

Ordinary accounts can only proxy traffic; calling these admin APIs returns
`PermissionDenied`.

The `api` subcommand connects through the config file's `outbound`, so the
outbound must be `shadowquic` or `sunnyquic` (`socks`/`direct` are not
supported).

## Can one port serve multiple users?

Yes. The server `users` list can contain any number of accounts, all sharing
the same `bind-addr` port. Each user gets independent traffic stats
(`api get-stats <name>`).

## Can I use it on my phone (Android/iOS)?

Yes, but the core program runs on the server + a computer/router:

- Android: pair it with clients like
  [nekobox](https://github.com/MatsuriDayo/nekobox), v2rayN — see the
  `document/clients/windows.md` in the repo.
- Or use any ready-made client with ShadowQUIC protocol support, e.g.
  [Clash-rs](https://github.com/Watfaq/clash-rs),
  [husi](https://github.com/xchacha20-poly1305/husi),
  [QuicProxy](https://github.com/RealBikiniBottom/QuicProxy), mihomo.

## Do I have to use the official client?

No. The protocol is public (see the `PROTOCOL` doc in the repo), and the
third-party clients listed above implement ShadowQUIC / SunnyQUIC. Official
cross-implementation interop test results are on the Interop page.

## Can I have multiple servers / load balancing?

ShadowQUIC itself is single-server. For multiple nodes and automatic
switching, use an upper-layer client like Clash-rs / mihomo, which can
group several ShadowQUIC nodes into a strategy pool.

## Something else is broken?

- Startup error: check the logs. Temporarily set `log-level: trace` for the
  most detailed output.
- Crash / weird behavior: search the repository
  [Issues](https://github.com/spongebob888/shadowquic/issues) — chances are
  someone hit it before. If not, file an issue with the logs.

## Related links

- Complete config reference: **Configuration** section in the navigation
  bar (auto-generated, in sync with the code)
- User management API: **API** section
- Protocol details: **Protocol** section (for the technically inclined)
