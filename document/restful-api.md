# REST API

Build with `cargo build --release --features restful-api`. This optional feature
enables statistics and adds an HTTP API alongside the proxy listeners. It is
enabled by default.
It is separate from the `api` CLI subcommand for remote user management.
The entire API module is compiled only with `restful-api`. The independent
`jemalloc` feature is enabled by default and selects the global allocator on
supported targets; enabling `restful-api` does not enable `jemalloc`.

Enable it in your proxy configuration:

```yaml
restful-api:
  tag: restful-api
  bind-addr: "127.0.0.1:9090"
  # ui-path: ./ui
  # secret: "choose-a-secret"
```

Any bind address is allowed, including `0.0.0.0` and `::`, with or without a
secret. A nonempty `secret` requires `Authorization: Bearer <secret>` on every
API request except CORS preflights. Omit `secret` or set it to `""` to disable
authentication. Omit `restful-api` to leave the HTTP listener disabled. Configuring
the listener without compiling `restful-api` produces a configuration error.
A bind failure prevents manager startup.

When a secret is set, include it in requests:

```sh
curl -H 'Authorization: Bearer choose-a-secret' http://127.0.0.1:9090/api/v1/traffic
```

Missing or invalid credentials return `401` with error code `unauthorized` and
`WWW-Authenticate: Bearer`.

A complete local SOCKS/direct example is available in
[restful-api.yaml](../shadowquic/config_examples/restful-api.yaml).

## Local dashboard

Set `ui-path` to a directory to serve a dashboard at the API listener's root,
for example `http://127.0.0.1:9090/`:

```yaml
restful-api:
  tag: dashboard-download
  bind-addr: "127.0.0.1:9090"
  ui-path: ./ui
  # ui-download-url: https://github.com/spongebob888/squirrelboard/releases/download/v0.1.2/index.html
```

If `ui-path/index.html` is missing, startup downloads Squirrelboard v0.1.2's
standalone `index.html`, with JavaScript and CSS bundled into that file. Override
`ui-download-url` with another HTTP(S) HTML file. The directory is created if
needed, and other existing files are preserved. Existing dashboards with an
`index.html` are served without downloading again. Omit `ui-path` to disable
both local UI serving and its startup download.

Downloads run in the background while the proxy and API start. The dashboard
root returns `503` until it is available. Download failures are logged and leave
the API running; restart to retry. Installation writes a temporary file and
publishes the complete `index.html` without replacing an existing page. Downloads
are limited to 32 MiB and ten HTTP requests through redirects.

The optional `tag` defaults to `restful-api` and must be nonempty and unique
among endpoint and database tags. Dashboard downloads, including each redirect,
carry it as `ctx.inbound_tag` through normal routing. For example:

```yaml
router:
  src: |
    return function(ctx)
      if ctx.inbound_tag == "dashboard-download" then
        return "direct"
      end
      return "proxy"
    end
```

Without a routing script, downloads use the global default outbound. Background
downloads do not appear in client connection or traffic statistics. Shutdown
cancels unfinished downloads.

Dashboard files accept unauthenticated `GET` and `HEAD` requests so the browser
can load the UI. API routes under `/api/` still require the configured secret;
enter that secret in the dashboard to connect. Extensionless dashboard routes
fall back to `index.html` for client-side navigation.

## Browser access (CORS)

CORS is enabled for all origins (`Access-Control-Allow-Origin: *`), including
on error responses. Browser preflight `OPTIONS` requests with `Origin` and
`Access-Control-Request-Method` headers return an empty `204` without requiring
authentication. They allow `GET`, `POST`, `PUT`, and `OPTIONS` with the
`Authorization` and `Content-Type` request headers. Actual API requests still
require the configured bearer token.

Use a bearer header for browser authentication; cookie credentials and
`fetch`'s `credentials: "include"` mode are not supported by this CORS policy.

## Endpoints

API responses use JSON with `snake_case` fields, except heap profile downloads.
Timestamps use UTC RFC 3339;
durations use milliseconds and byte counters use bytes. Unavailable optional
fields are `null`. Collections are sorted by tag or session ID.

| Method | Path | Response |
| --- | --- | --- |
| GET | `/api/v1/outbounds` | `{"outbounds": [...]}`: tags and cached connection/probe quality |
| GET | `/api/v1/connections` | `{"connections": [...]}`: active TCP sessions and UDP associations |
| GET | `/api/v1/traffic` | `{"inbounds": [...]}`: summed user traffic and active session counts per inbound |
| GET | `/api/v1/tokio_metric` | Tokio runtime worker count, alive task count, and global queue depth |
| GET | `/api/v1/memory` | jemalloc allocator statistics; requires `jemalloc` |
| POST | `/api/v1/memory/profile` | Download a jemalloc heap profile; requires `mem-profile` |
| GET | `/api/v1/memory/profile/status` | Current sampling state as `{"active":true}` or `{"active":false}`; requires `mem-profile` |
| POST | `/api/v1/memory/profile/activate` | Resume heap sampling; returns `{"active":true}`; requires `mem-profile` |
| POST | `/api/v1/memory/profile/deactivate` | Pause heap sampling; returns `{"active":false}`; requires `mem-profile` |
| POST | `/api/v1/outbounds/{tag}/probe` | Probe the selected outbound with an HTTP GET |
| GET | `/api/v1/outbounds/{tag}/selection` | Selector's current member and allowed outbound tags |
| PUT | `/api/v1/outbounds/{tag}/selection` | Set a selector's member with `{"selected":"outbound-tag"}` |

```sh
curl http://127.0.0.1:9090/api/v1/outbounds
curl http://127.0.0.1:9090/api/v1/connections
curl http://127.0.0.1:9090/api/v1/traffic
curl http://127.0.0.1:9090/api/v1/tokio_metric
curl http://127.0.0.1:9090/api/v1/memory
curl -H 'Content-Type: application/json' \
  -d '{"url":"http://example.com/","timeout_ms":10000}' \
  http://127.0.0.1:9090/api/v1/outbounds/direct/probe
```

Percent-encode the outbound tag as one path segment when necessary.
Errors have the form `{"error":{"code":"...","message":"..."}}`.
Invalid requests return `400`, unknown routes or tags `404`, wrong methods
`405`, overlapping probes on the same outbound `409`, unsupported operations
`501`, failed probes `502`, and probe timeouts `504`.

## Tokio runtime metrics

`GET /api/v1/tokio_metric` (`get_tokio_metric`) returns metrics from the Tokio
runtime serving the request:

```json
{"num_workers": 8, "num_alive_tasks": 42, "global_queue_depth": 0}
```

`num_workers` is the runtime's worker thread count (always `1` for a
current-thread runtime). `num_alive_tasks` counts tasks that have been spawned
and have not yet exited, including API and background tasks. `global_queue_depth`
counts tasks waiting in the global scheduler queue, excluding worker-local
queues. These are live measurements and may change while the response is built.

## Selector outbounds

Use a selector as the router's outbound, or an inbound's `default-outbound`,
to switch between configured outbounds at runtime:

```yaml
outbounds:
  - tag: choice
    type: selector
    outbounds: [direct, proxy]
    selected: direct
  - tag: direct
    type: direct
  - tag: proxy
    type: socks
    addr: "127.0.0.1:1080"
router:
  default-outbound: choice
```

`outbounds` must contain at least one unique, existing outbound tag. `selected`
defaults to its first member and must name a member when specified. Members can
be declared later in the configuration and can include other selectors or DNS
services. Self-references and cycles through any members are rejected at startup.
Each member retains its own DNS and transport settings.

```sh
curl http://127.0.0.1:9090/api/v1/outbounds/choice/selection
curl -X PUT -H 'Content-Type: application/json' \
  -d '{"selected":"proxy"}' \
  http://127.0.0.1:9090/api/v1/outbounds/choice/selection
```

Both operations return `{"selected":"proxy","outbounds":["direct","proxy"]}`
when `proxy` is selected. The outbound listing includes the same object in a
selector entry's `selector` field. Non-selector entries omit that field.
The usual bearer authentication applies. Unknown tags return `404`; selecting
a nonmember or using selection controls on a non-selector returns `400`.

A switch applies to new TCP sessions, UDP associations, and probes that dispatch
after the switch. Existing sessions keep their original target, including all
datagrams in an existing UDP association. Nested selectors resolve their own
selection when they receive the session. Switches are held in memory; restarting
restores the configured initial selection. Connection statistics retain the
selector tag chosen by routing. Probe results remain cached until another probe
completes, so an earlier result may describe a previously selected member.
The selector itself is available without `restful-api`; changing it through
HTTP requires that feature and a configured API listener. Library embeddings
can import `shadowquic::Selectable` and call `select` directly. Selection controls
are independent of `Outbound`: `Config::build_manager` registers the same
selector instances in `manager.selectable_outbounds` as `Arc<dyn Selectable>`.
Embeddings constructing a manager manually register selection controls in that
map under the corresponding outbound tag to expose them through the REST API.

## Connections and traffic

Observers cover ShadowQUIC, SunnyQUIC, SOCKS, mixed, TPROXY, and TUN inbounds
when the corresponding features and platform support are available. Tracking
does not require an authenticated user. Internal DNS upstream and database
download queues are excluded from client tracking.
`Config::build_manager` creates `manager.observers` alongside the inbound map,
sharing each server's own observer and omitting internal queues. SOCKS, mixed
(SOCKS and HTTP), TPROXY, and TUN attach their observers to accepted TCP and UDP
sessions, just like the QUIC servers. Tracking starts after routing and fake-IP
restoration, so a session is counted once with its final destination.
SOCKS and mixed requests carry their authenticated username in `StatsContext`.
Anonymous SOCKS/mixed requests and TUN/TPROXY requests use username `""` and
an empty connection handle.

Each connection contains `id`, `protocol`, `inbound`, `outbound`, `src`,
`dst_ip`, `dst_domain`, `dst_port`, `upload_bytes`, `download_bytes`, `duration_ms`,
`first_response_ms`, and `half_closed`. The opaque ID is stable during a
manager's lifetime.
`src` includes the client port; IP fields do not include ports. Destinations
reflect fake-IP restoration and router rewrites. IP destinations can have a
cached domain alongside the IP; listing sessions never performs DNS queries.

`upload_bytes` counts client uploads, and `download_bytes` counts downloads to the
client. Counters include payload bytes actually read or written, excluding
protocol framing. `first_response_ms` measures from tracking creation after
routing to the first nonempty response sent to the client; it is `null` before
that event. A UDP entry describes the association's initial routed destination
and remains active while either tracked half exists. `half_closed` is `true`
when exactly one scoreboard weak pointer can be upgraded. Fields owned by an
expired half become `null`: receive stats supply `src`, `upload_bytes`, and
`duration_ms`; send stats supply `outbound`, destination fields, and `download_bytes`.
`first_response_ms` requires both halves. The ID stays the same whichever half
survives. Entries disappear once both weak pointers have expired.
`duration_ms` is the elapsed time since tracking began, measured with `Instant`.

Traffic entries contain `inbound`, `tcp_upload_bytes`, `tcp_download_bytes`,
`udp_upload_bytes`, `udp_download_bytes`, `tcp_conns`, and `udp_conns`.
These are sums of all `user_stats` entries; anonymous requests
use username `""`. TCP and UDP bytes are reported separately and survive session completion.
Restored user traffic is included, while resetting or removing user counters
changes the totals accordingly. Session counts follow the lifetime of the
receive stats, so a send-only half-closed entry can remain listed with a count
of zero. Concurrent snapshots
may differ slightly as I/O progresses.

## Outbound quality and probes

Each outbound has a `quality` object with nullable `sq_conn_stats` and `probe`
fields. QUIC samples contain `uplink`, `downlink`, and `sample_age_ms`.
Each direction exposes `lost_packets`, `sent_packets`, `rtt` in milliseconds,
and `current_mtu` in bytes. Samples are absent for unsupported, disconnected,
and unsampled connections. Listing quality does not establish a connection or
request fresh statistics.

Probes accept absolute `http://` URLs without credentials; HTTPS is unsupported
in this version. `timeout_ms` defaults to 10000 and must be between 1 and 60000.
The selected outbound must support TCP (direct, SOCKS, ShadowQUIC, SunnyQUIC,
or a selector pointing to one of these).
Routing scripts do not select or override the probe's outbound.

Latency includes resolution, connection setup, and HTTP request transmission
through complete response headers. The response body is not downloaded.
Only `2xx` final responses succeed; redirects are not followed. Each outbound
allows one probe at a time. Probes do not affect client connections or traffic
totals, and their streams are closed on completion or cancellation.

The latest completed probe is cached with `url`, `status`, `latency_ms`,
`measured_at`, and `error`. Status is `ok`, `failed`, or `timeout`. Failure
replaces the old latency with `null` and records an error. Invalid, unsupported,
conflicting, and cancelled requests leave the cache unchanged. Cached results
remain until replaced; use `measured_at` to assess freshness.

## Memory statistics and the global allocator

The `jemalloc` feature is enabled by default. On Linux and macOS, for x86_64
and aarch64, and on Android (aarch64, armv7, i686, and x86_64), it installs
jemalloc as the global allocator for both the CLI and library embeddings,
independently of `restful-api`. Use `--no-default-features`
and select the required protocol features without `jemalloc` to use the system
allocator. An embedding supplying its own global allocator must disable the
crate's `jemalloc` feature to avoid conflicting allocator definitions.

The repository's Cargo configuration sets the jemalloc allocator page size to
16 KB for aarch64 and x86_64 Android, covering both 4 KB and 16 KB system pages.
When embedding from another workspace, set
`AARCH64_LINUX_ANDROID_JEMALLOC_SYS_WITH_LG_PAGE=14` or
`X86_64_LINUX_ANDROID_JEMALLOC_SYS_WITH_LG_PAGE=14` in that workspace's build
environment too. See Android's [16 KB page-size guidance](https://developer.android.com/guide/practices/page-sizes).

The memory endpoint is compiled only when both `restful-api` and `jemalloc`
are enabled. Without `jemalloc`, `/api/v1/memory` returns `404`.
Memory responses contain `allocator: "jemalloc"`,
`allocated_bytes`, `active_bytes`, and `resident_bytes`. These are allocator
statistics, not total process RSS. With both features, other targets return `501` with
`memory_stats_unavailable`; the other endpoints remain available.

### Heap profiling

The `mem-profile` feature is disabled by default. It enables `restful-api`,
`jemalloc`, and jemalloc's profiling support. On targets supported by the
jemalloc allocator above, it enables allocation sampling from process startup
with jemalloc's default sampling interval (512 KiB). Profiling adds CPU and
memory overhead while the process runs.

```sh
./scripts/build-profiling.sh
# Run with a configuration that enables the REST API listener.
./target/release/shadowquic -c shadowquic/config_examples/restful-api.yaml
```

The build script uses the release profile with full debug symbols and stripping
disabled. By default, it enables both `mem-profile` and `tokio-console`, and sets
`--cfg tokio_unstable` for Tokio console instrumentation. Use
`./scripts/build-profiling.sh --mem-profile-only` to enable only `mem-profile`
without adding Tokio console instrumentation; Cargo's default features remain
enabled. Existing `RUSTFLAGS` or `CARGO_ENCODED_RUSTFLAGS` are preserved;
additional arguments are passed to Cargo. For `--target x86_64-unknown-linux-musl`
(also accepted via `CARGO_BUILD_TARGET`), the script uses `cargo rustc` with the
Rust toolchain's bundled LLVM linker to avoid GNU linker errors in debug sections
with fat LTO. The musl C compiler and linker must still be configured, for example
by entering `nix develop` first:

```sh
./scripts/build-profiling.sh --mem-profile-only --target x86_64-unknown-linux-musl
```

Tokio console retains completed data for 60 seconds by default;
override this with `TOKIO_CONSOLE_RETENTION`, for example `120s`.

In another terminal, trigger a snapshot and save it locally:

```sh
curl --fail -X POST http://127.0.0.1:9090/api/v1/memory/profile \
  -o shadowquic.heap
# Add -H 'Authorization: Bearer choose-a-secret' when a secret is configured.
```

Sampling is active by default when built with `mem-profile`. Pause or resume it
without restarting the process:

```sh
curl --fail http://127.0.0.1:9090/api/v1/memory/profile/status
# {"active":true}
curl --fail -X POST http://127.0.0.1:9090/api/v1/memory/profile/deactivate
# {"active":false}
curl --fail -X POST http://127.0.0.1:9090/api/v1/memory/profile/activate
# {"active":true}
```

These process-wide controls require no request body and use the existing bearer
authentication. Repeated calls to the same action succeed. Deactivation stops
sampling new allocations; existing samples continue to track deallocations and
remain available through the dump endpoint. Activation resumes sampling without
resetting previous data. Allocations made while sampling is inactive are not
added retroactively. Deactivation does not purge allocator pages or guarantee
that RSS decreases.

The status endpoint reads the allocator's current state without changing it,
including after activation or deactivation. Status reads remain available during
profile dumps.

The response is `application/octet-stream` with attachment filename
`shadowquic.heap`. It contains jemalloc's native heap profile, including sampled
allocation stacks and mapped libraries. Analyze it with `jeprof` and the exact
executable used for the capture. The build command above retains debug symbols
for function names; the repository's usual release build strips them.
The server uses a temporary file and removes it after reading the snapshot.
Profiling requests run on blocking workers. Overlapping dumps and sampling
controls return `409` with
`memory_profile_in_progress`.

Without `mem-profile` these routes return `404`; unsupported targets return `501`
with `memory_profiling_unavailable`. `_RJEM_MALLOC_CONF` can override jemalloc's
startup settings. If `prof:false` disables profiling, these endpoints return
`503` with `memory_profiling_disabled`; restart with
`prof:true,prof_active:true` to enable profiling. `prof_active:false` starts with
sampling paused and allows activation through the API. All requests use the
API's existing authentication and CORS rules.

## Library integration

For a manager created directly, set `manager.api_config` to
`Some(ApiConfig { bind_addr, secret: None })` before calling `run` or `run_until`.
Use `secret: Some("your-secret".into())` to require bearer authentication.
Each inbound attaches its observer in `accept`; the manager uses the attached
observer after routing. `Manager::single` leaves the observer registry empty.
For a manually created built-in server, clone `server.observer` before boxing
the server and put that `Arc` in `manager.observers` under its inbound tag to
expose its statistics through the API. Custom inbounds attach their own observer
in `accept` and register the same `Arc`. Omit internal request queues from this
registry.
`Config::build_manager` also populates `manager.sq_outbounds` with
`Arc<dyn SQStatsTrait>` references to the original QUIC clients. Embeddings
building the outbound map manually can populate this map to expose statistics.
Listing and probing use the manager's existing outbound list with DNS service
outbounds removed. Probes dispatch an in-memory TCP session through
`Outbound::handle`, including configured DNS wrappers. Custom outbounds require
no additional probing trait or registration.
