# Changelog

Notable changes to Shadowquic, newest first. This history is reconstructed from
Git tags and commits; dates are the dates of the tagged commits. Routine version
bumps, formatting, and dependency updates are omitted unless they affect behavior.
Version links compare each release with the preceding tag.

## [0.4.10](https://github.com/spongebob888/shadowquic/compare/v0.4.9...v0.4.10) - 2026-11-08

### Added

- Structured log event names and global request IDs for connection timelines,
  including DNS upstream request correlation and router destination rewrites.
- Live structured logs over WebSocket at `GET /api/v1/logs`.
- A standalone HTML log viewer with filtering, request timelines, import/export,
  and IndexedDB persistence with configurable retention.
- Documentation and examples for hostname sniffing, destination rewriting,
  router databases, and DHCP lease lookups.

### Fixed

- Correct log categories, missing direct traffic statistics, duplicate inbound
  spans, and request ID propagation through TPROXY.
- Initialize log viewer storage when served over plain HTTP.
- Increase the inbound SQ UDP channel capacity.

## [0.4.9](https://github.com/spongebob888/shadowquic/compare/v0.4.8...v0.4.9) - 2026-11-07

### Added

- Configurable HTTP, TLS, and QUIC hostname inspection before Lua routing.
  The `sniffer` feature is included in default builds; inspection requires
  `router.sniffer` configuration and a routing script.
- QUIC ClientHello reassembly across up to five datagrams, with inspection
  results and elapsed time available in debug logs.
- Sniffer metadata in REST API connection snapshots and SQ connection duration
  measured from connection creation.

### Fixed

- Accept trailing zero padding in QUIC Initial datagrams during inspection.
- Expose consistent lowercase sniffer protocol names.
- Restore macOS ARM release builds with a supported CI runner.

## [0.4.8](https://github.com/spongebob888/shadowquic/compare/v0.4.7...v0.4.8) - 2026-11-05

### Changed

- Batch TUN ingress and amortize session cleanup.
- Compact redb databases after imports and rebuilds to reclaim unused space.
- Correct UDP benchmark measurements and add direct transport benchmarks.
- Sample SQ packet trace logging.

### Fixed

- Preserve UDP receive ordering and keep the receiver running after individual
  packet delivery errors.

## [0.4.7](https://github.com/spongebob888/shadowquic/compare/v0.4.6...v0.4.7) - 2026-10-29

### Added

- Separate TCP and UDP idle timeouts for TUN inbounds.
- Optional TCP keepalive for direct outbounds.
- Geosite schema 3 with attribute bitmasks and legacy schema detection.

### Fixed

- Prevent full UDP channels from blocking packet dispatch, including TPROXY.
- Close direct UDP sessions when their controlling stream closes.
- Apply a ten-second timeout to outbound connection preparation.
- Drive REST API HTTP probes alongside outbound sessions.
- Normalize SQ source addresses and increase DNS channel capacity.
- Restore i686 socket checks in CI on Ubuntu 26.04.

## [0.4.6](https://github.com/spongebob888/shadowquic/compare/v0.4.5...v0.4.6) - 2026-10-25

### Added

- Serve a local dashboard through the REST API listener, with automatic startup
  download when the configured UI directory has no `index.html`.
- Configurable ShadowQUIC blackhole detection threshold.

### Fixed

- Avoid blocking runtime threads during direct outbound DNS resolution and use
  the DNS API for legacy direct resolution.
- Resolve the first destination handled by a resolving outbound.
- Handle SQ UDP bind failures without panicking and avoid holding a lock across
  an await.
- Bound reads of SQ protocol extensions.
- Close completed startup download inbounds and omit empty usernames from logs.
- Update certificate trust dependencies to distrust eMudhra CAs.

## [0.4.5](https://github.com/spongebob888/shadowquic/compare/v0.4.4...v0.4.5) - 2026-10-21

### Added

- Optional TUN inbound with fake-IP destination restoration, including Android
  support and the ability to disable IPv6.
- Local REST API with optional bearer authentication, browser CORS support,
  connection and traffic snapshots, and outbound selection.
- Per-connection statistics, Tokio runtime metrics, and optional jemalloc heap
  profile downloads and sampling controls.
- DNS-over-QUIC upstreams with opt-in zero-RTT support.
- Configurable QUIC stream and flow-control window limits.
- Configurable fake-IP address ranges.

### Changed

- Bound DNS response and fake-IP caches with `quick_cache`.
- Change the default IPv6 fake-IP range to `2001:2::/48`.

### Fixed

- Wrap fake-IP allocation safely and skip indices still held by cache entries.
- Retain the DNS UDP endpoint across QUIC reconnects.

## [0.4.4](https://github.com/spongebob888/shadowquic/compare/v0.4.3...v0.4.4) - 2026-10-12

### Added

- DHCP lease lookups in Lua routing, with automatic lease file reloads.
- TPROXY route setup and cleanup scripts.

### Changed

- Optimize release binary size.

## [0.4.3](https://github.com/spongebob888/shadowquic/compare/v0.4.2...v0.4.3) - 2026-10-11

### Added

- Direct, memory-mapped Country MMDB lookups as an alternative to converted redb
  databases.
- Indexed Geosite lookups with stored sparse regex DFAs and import filtering
  through `list-include`.
- TCP DNS hijacking.
- IP and Geosite lookup benchmarks.

### Fixed

- Add an inactivity timeout for half-closed outbound TCP connections.
- Canonicalize dual-stack SOCKS inbound addresses.
- Keep request fields off listener log spans and report request peers consistently.

## [0.4.2](https://github.com/spongebob888/shadowquic/compare/v0.4.1...v0.4.2) - 2026-10-07

### Added

- Socket options for direct outbounds.
- Per-server DNS cache bypass.
- A TPROXY configuration example.

### Changed

- Store country database IP ranges compactly and merge adjacent ranges.

### Fixed

- Isolate fake-IP resolution from the shared DNS cache.
- Canonicalize IPv4 addresses in TPROXY.

## [0.4.1](https://github.com/spongebob888/shadowquic/compare/v0.4.0...v0.4.1) - 2026-10-05

### Added

- DNS services with managed resolvers, shared caching, reverse lookups, fake-IP
  support, and outbound domain resolution.
- Asynchronous Lua routing with DNS lookup helpers and DNS query context.
- Router databases for country and Geosite lookups, behind the `router-db` feature.
- Per-inbound `default-outbound` selection.
- Static musl builds through `nix build .#musl`.

### Fixed

- Reject unsupported UDP destination rewrites from router scripts.

## [0.4.0](https://github.com/spongebob888/shadowquic/compare/v0.3.13...v0.4.0) - 2026-09-30

### Added

- Multiple tagged `inbounds` and `outbounds`, with a configurable default outbound.
- Lua routing through inline source or script files, with source, destination,
  network, and inbound tag context.
- Automatic router script reloads, TCP destination rewriting, and a drop outbound.
- Router execution timing in logs.

### Changed

- Plural endpoint lists require nonempty, unique tags. Existing singular
  `inbound` and `outbound` configurations remain supported, with default tags
  when omitted.
- Debounce connection statistics per connection and improve tracing across tasks.

### Fixed

- Retry server accept errors with backoff.
- Normalize IP literals passed as SOCKS domain targets.
- Fall back to a single-stack direct UDP socket if dual-stack binding fails.
- Disable Nagle's algorithm on inbound TCP connections.

## [0.3.13](https://github.com/spongebob888/shadowquic/compare/v0.3.12...v0.3.13) - 2026-08-29

### Added

- Persist users and traffic statistics in a user-store YAML file.
- `api clear-stats` to reset per-user traffic statistics.
- Statistics on 32-bit targets through `portable-atomic`.

### Fixed

- Restore MIPS target builds.

## [0.3.12](https://github.com/spongebob888/shadowquic/compare/v0.3.11...v0.3.12) - 2026-07-18

### Added

- Mixed HTTP/SOCKS5 inbound listener behind the `mixed` feature, with HTTP proxy
  authentication and concurrent connection handling.
- Release build coverage for LoongArch64.

### Fixed

- Match HTTP `Proxy-Authorization` schemes case-insensitively and bracket IPv6
  literals in forwarded `Host` headers.
- Correct dual-stack binding for `0.0.0.0` and Windows IPv6 sockets.
- Set TCP_NODELAY for direct outbounds and SOCKS inbounds.
- Update rustls-jls to fix HelloRetryRequest handling.

## [0.3.11](https://github.com/spongebob888/shadowquic/compare/v0.3.10...v0.3.11) - 2026-06-10

### Added

- Linux TPROXY inbound with idle UDP session cleanup.
- API access to statistics for all users.
- Shorter encoded lengths with the procedural macro `size_tag` option.

## [0.3.10](https://github.com/spongebob888/shadowquic/compare/v0.3.9...v0.3.10) - 2026-06-07

### Fixed

- Resolve deadlocks in connection and statistics handling.

## [0.3.9](https://github.com/spongebob888/shadowquic/compare/v0.3.8...v0.3.9) - 2026-06-07

### Added

- Per-user traffic and connection statistics, API commands for statistics, and
  connection termination support.
- A compile-time option to disable statistics.

### Changed

- Accept SOCKS connections concurrently.
- Rename the SunnyQUIC iroh backend to noq.

### Fixed

- Correct missing-user error reporting and fallback behavior.

## [0.3.8](https://github.com/spongebob888/shadowquic/compare/v0.3.7...v0.3.8) - 2026-06-06

### Added

- Remote user management APIs and CLI commands for ShadowQUIC and SunnyQUIC.
- Runtime configuration updates through the SQ protocol API.
- Authenticated usernames in request context; usernames beginning with `admin`
  can perform administrative operations.

### Fixed

- Correct atomic zero-RTT state handling.

## [0.3.7](https://github.com/spongebob888/shadowquic/compare/v0.3.6...v0.3.7) - 2026-05-28

### Added

- An option to disable blackhole detection.

### Fixed

- Skip binding sockets to an interface for loopback destinations.
- Improve statistics formatting and tracing detail.

## [0.3.6](https://github.com/spongebob888/shadowquic/compare/v0.3.5...v0.3.6) - 2026-05-24

### Added

- Socket options and interface binding for QUIC and SOCKS clients.
- SQ peer statistics API and protocol extension support.
- Procedural macro support for size tags.
- Generated configuration documentation.

### Fixed

- Debounce statistics logging.

## [0.3.5](https://github.com/spongebob888/shadowquic/compare/v0.3.4...v0.3.5) - 2026-05-15

### Added

- `aws-lc-rs` as an alternative to the `ring` cryptography backend.

### Fixed

- Correct UDP-over-datagram ordering.
- Handle changes in QUIC address family while running.

## [0.3.4](https://github.com/spongebob888/shadowquic/compare/v0.3.3...v0.3.4) - 2026-04-12

### Added

- Configurable Brutal congestion control for ShadowQUIC.
- Configurable TLS cipher suite preference for devices with limited CPU resources.

### Changed

- Use a hardened `shadowquic@.service` systemd template with `DynamicUser`;
  the installer uses the `server` instance.

## [0.3.3](https://github.com/spongebob888/shadowquic/compare/v0.3.2...v0.3.3) - 2026-02-13

### Added

- Enum encoding and decoding in procedural macros, including integer constant
  discriminants.
- Public `SEncode` and `SDecode` traits and a `ShadowQuicConn` alias.

### Changed

- Encoding takes `&self`.

## [0.3.2](https://github.com/spongebob888/shadowquic/compare/v0.3.1...v0.3.2) - 2026-02-08

### Changed

- Use published crate dependencies for packaging.

### Fixed

- Improve YAML error messages and reject unknown configuration fields.

## [0.3.1](https://github.com/spongebob888/shadowquic/compare/v0.3.0...v0.3.1) - 2026-02-08

### Added

- Certificate hot reload for SunnyQUIC.

### Fixed

- Correct client certificate handling.

## [0.3.0](https://github.com/spongebob888/shadowquic/compare/v0.2.3...v0.3.0) - 2026-02-05

### Added

- SunnyQUIC transport with multipath support and configuration examples.
- Configurable MTU discovery and QUIC generic segmentation offload (`gso`).
- Linux installation script.

### Fixed

- Correct SunnyQUIC authentication, certificate validation, and multipath
  interaction with zero-RTT connections.

## [0.2.3](https://github.com/spongebob888/shadowquic/compare/v0.2.2...v0.2.3) - 2026-01-09

### Added

- Nix build support and MIPS release targets.
- v2rayN client documentation.

### Fixed

- Enable TCP_NODELAY for SOCKS outbounds.
- Restore x86_64 macOS builds.

## [0.2.2](https://github.com/spongebob888/shadowquic/compare/v0.2.1...v0.2.2) - 2025-09-07

### Added

- Configurable DNS resolution strategy for direct outbounds.
- Docker Compose and Nekobox usage documentation.

## [0.2.1](https://github.com/spongebob888/shadowquic/compare/v0.2.0...v0.2.1) - 2025-07-20

### Fixed

- Update quinn-jls to address issue #52 and correct Quinn logging.
- Align the server example with multi-user configuration.

## [0.2.0](https://github.com/spongebob888/shadowquic/compare/v0.1.15...v0.2.0) - 2025-07-13

### Added

- Multi-user authentication and rate limiting.

### Fixed

- Apply a default rate limit and retry socket protection operations.

## [0.1.15](https://github.com/spongebob888/shadowquic/compare/v0.1.14...v0.1.15) - 2025-07-08

### Added

- Dual-stack SOCKS server support.
- Windows client usage documentation.

## [0.1.14](https://github.com/spongebob888/shadowquic/compare/v0.1.13...v0.1.14) - 2025-06-29

### Changed

- Refactor the QUIC abstraction to use generic traits and standalone QUIC errors.

### Fixed

- Restore Android builds.

## [0.1.13](https://github.com/spongebob888/shadowquic/compare/v0.1.12...v0.1.13) - 2025-06-19

### Added

- FreeBSD release target.

### Fixed

- Set dual-stack socket options before binding.

## [0.1.12](https://github.com/spongebob888/shadowquic/compare/v0.1.11...v0.1.12) - 2025-06-14

### Added

- Socket protection support and a helper script.
- Android release targets.

### Fixed

- Handle IPv6 wildcard bind failures on IPv4-only systems.

## [0.1.11](https://github.com/spongebob888/shadowquic/compare/v0.1.10...v0.1.11) - 2025-05-20

### Fixed

- Keep UDP sessions alive while retrieving their sockets.

## [0.1.10](https://github.com/spongebob888/shadowquic/compare/v0.1.9...v0.1.10) - 2025-05-17

### Fixed

- Prevent a panic when retrieving a socket immediately after its session drops.

## [0.1.9](https://github.com/spongebob888/shadowquic/compare/v0.1.8...v0.1.9) - 2025-05-17

### Fixed

- Prevent UDP receivers from being dropped prematurely.

## [0.1.8](https://github.com/spongebob888/shadowquic/compare/v0.1.7...v0.1.8) - 2025-05-16

### Fixed

- Clean up UDP context ID storage correctly.

## [0.1.7](https://github.com/spongebob888/shadowquic/compare/v0.1.6...v0.1.7) - 2025-05-10

### Added

- SOCKS5 inbound and outbound authentication.

## [0.1.6](https://github.com/spongebob888/shadowquic/compare/v0.1.5...v0.1.6) - 2025-05-02

### Added

- Configurable keepalive interval.
- Docker image builds.

## [0.1.5](https://github.com/spongebob888/shadowquic/compare/v0.1.4...v0.1.5) - 2025-05-01

### Added

- SOCKS5 outbound support and clearer control stream errors.

## [0.1.4](https://github.com/spongebob888/shadowquic/compare/v0.1.3...v0.1.4) - 2025-04-29

### Added

- clash-rs compatibility.

## [0.1.3](https://github.com/spongebob888/shadowquic/compare/v0.1.2...v0.1.3) - 2025-04-28

### Changed

- Increase transport send and receive windows.

### Fixed

- Release UDP sockets when sessions close.

## [0.1.2](https://github.com/spongebob888/shadowquic/compare/v0.1.1...v0.1.2) - 2025-04-27

### Added

- CLI version command.

### Fixed

- Close UDP sessions correctly.

## [0.1.1](https://github.com/spongebob888/shadowquic/compare/v0.1.0...v0.1.1) - 2025-04-23

### Changed

- Send destination/context ID pairs only once and increase the minimum MTU.

## [0.1.0](https://github.com/spongebob888/shadowquic/releases/tag/v0.1.0) - 2025-04-21

### Added

- Initial ShadowQUIC client and server with SNI camouflage, zero-RTT connections,
  TCP proxying, UDP over QUIC datagrams or streams, and full-cone UDP support.
- Direct outbound and SOCKS inbound support, configuration parsing, IPv4-only mode,
  and Windows dual-stack handling.
- Protocol documentation, TCP/UDP benchmarks, automated tests, and release builds.
