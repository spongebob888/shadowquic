# REST API

## Scope and configuration

Implement an optional HTTP API for outbound quality, active proxy sessions,
traffic counters, Tokio runtime metrics, memory statistics, and outbound probing.

- Gate the API and its dependencies behind the default-enabled `restful-api` Cargo
  feature. Enabling it must automatically enable `statistics`.
- Use an independent `jemalloc` feature, enabled by default, to select jemalloc
  as the global allocator on supported targets. `restful-api` must not enable it.
  Compile the memory endpoint only when both features are enabled.
- Start the API only when `restful-api` is present. Reject that configuration
  when the binary was built without `restful-api`.
- Require `bind-addr` and allow any address, including wildcard addresses,
  whether or not a secret is configured.
- Accept an optional `secret`. If nonempty, require `Authorization: Bearer <secret>`
  on every request except CORS preflights before route validation or probe execution.
  Compare tokens in constant time. Reject missing, malformed, duplicate, or incorrect credentials
  with `401`, error code `unauthorized`, and `WWW-Authenticate: Bearer`.
  Omitted or empty secrets disable authentication. Never return the secret in errors.
- Run the API alongside the proxy listeners. A bind failure must fail startup;
  shut down the API and cancel probes when the manager shuts down.

```yaml
restful-api:
  bind-addr: "127.0.0.1:9090"
  # secret: "choose-a-secret"
```

## Observer coverage and metric semantics

Preserve observer support for ShadowQUIC and SunnyQUIC, and add support for
mixed, TPROXY, and TUN inbounds. Include standalone SOCKS inbounds through the
same request tracking path. Preserve existing feature and platform gates.

Create `manager.observers: HashMap<String, Arc<Observer>>` alongside the inbound
map, registering only client inbounds. ShadowQUIC, SunnyQUIC, SOCKS, mixed,
TPROXY, and TUN servers each own an `Arc<Observer>` and attach its weak reference
to every accepted TCP or UDP request. Clone that same `Arc` into the map before
erasing the concrete inbound type; keep it when wrapping an inbound for fake-IP
restoration. Track requests once, after routing. Do not add observer or
tracking methods to `Inbound`.
Keep manager registration and dispatch independent of the `statistics` feature;
the observer module supplies the empty implementation when it is disabled.

Pass this observer map to `ApiManager` by inbound tag. Requests without an
authenticated user must still be tracked. Store their counters under the empty
username `""` in `user_stats`, without creating a QUIC connection handle.
Preserve existing authenticated user statistics and connection control behavior.
SOCKS and mixed inbounds must populate `UserContext.stats` with the username
accepted by SOCKS password or HTTP Basic authentication, or `""` when anonymous.
TUN and TPROXY also populate it with `""`. Use the default empty connection
handle for these local inbounds; only live handles belong in `sq_conns`.

Internal DNS upstream and database download queues are excluded from client
tracking so probes and background work do not inflate proxy-session metrics.
Their tags have no entries in the observer map. The manager preserves the
observer attached by `Inbound::accept`; it does not attach observers from the
registry to requests. The registry is used to expose observer snapshots.

In this API, a connection means one TCP proxy session or UDP association, not
the underlying QUIC transport connection. A UDP association can carry packets
for multiple destinations; its reported destination is the initial routed
destination. Do not create duplicate entries for cloned UDP send handles.

- All byte counters count successfully transferred proxy payload bytes, excluding
  protocol framing. Partial I/O counts only the bytes actually transferred;
  errors and pending operations do not increase counters.
- `upload_bytes` means bytes received from the inbound client (upload).
  `download_bytes` means bytes sent back to the inbound client (download).
  Traffic totals expose these separately with `tcp_` and `udp_` prefixes.
- Create session metadata when tracking starts, after routing selects the
  outbound. Thus creation time excludes inbound handshakes and router execution.
- `first_response_ms` is the monotonic duration from tracking creation to the
  first successful, nonempty write or datagram sent back to the client. It is
  `null` until that event and is set only once. TCP vectored writes count too.
- Calculate API traffic totals by summing all entries in `user_stats`, including
  the anonymous `""` entry. Completed sessions remain included in byte totals.
  Restored traffic is included; resetting or removing user counters changes the
  totals accordingly. Do not maintain separate `overall` counters or a
  `TrackedSession` wrapper.
- A TCP session is active while its tracked stream exists. A UDP association
  remains active while either tracked half exists. Each half owns only its own
  stats; do not retain the other half's stats or maintain an `active` field.
  Determine liveness by upgrading the scoreboard's weak pointers. Report
  `half_closed: true` when exactly one upgrades, and omit entries when neither
  upgrades. Data owned by an expired half is `null` in the API.
- Store the user's `tcp_conns` or `udp_conns` counter in `TrackedRecvStats`.
  Increment it on creation and decrement it on drop. Tracking the receive half
  is sufficient; send stats need no connection counter or shared counter guard.
  A send-only half-closed entry can remain listed with a connection count of zero.
- Snapshots may contain small differences caused by concurrent I/O; they need
  not be atomic across all observers. Reading a snapshot must not hold observer
  locks during network I/O or JSON serialization.

## HTTP conventions

Use `/api/v1` as the route prefix. Requests and responses use JSON with
`Content-Type: application/json`. Field names use `snake_case`.

Timestamps are UTC RFC 3339 strings. Durations are nonnegative milliseconds;
byte and connection counters are unsigned integers. Unknown optional values
are explicitly `null`. IP fields contain address literals without ports;
socket-address fields include ports and use brackets for IPv6.

| Operation | Method and path | Success response |
| --- | --- | --- |
| `get_outbounds` | `GET /api/v1/outbounds` | `{"outbounds": [...]}` |
| `get_connections` | `GET /api/v1/connections` | `{"connections": [...]}` |
| `get_overall_traffic` | `GET /api/v1/traffic` | `{"inbounds": [...]}` |
| `get_tokio_metric` | `GET /api/v1/tokio_metric` | `{"num_workers": N, "num_alive_tasks": N, "global_queue_depth": N}` |
| `get_mem_usage` (requires `jemalloc`) | `GET /api/v1/memory` | Memory object |
| `probe_outbound` | `POST /api/v1/outbounds/{tag}/probe` | Probe object |
| `get_selection` | `GET /api/v1/outbounds/{tag}/selection` | Selector state |
| `set_selection` | `PUT /api/v1/outbounds/{tag}/selection` | Selector state |

Successful requests return `200`. Empty collections return an empty array.
Sort outbound and traffic entries by tag, and connections by ID.

Errors use `{"error": {"code": "invalid_request", "message": "..."}}`.
Return `400` for invalid input, `401` for missing or invalid authentication when
a secret is configured, `404` for unknown routes or outbound tags, `405`
for unsupported methods, `409` for an already-running probe on the same outbound,
`501` for an unsupported operation, `502` for a failed probe, `504` for a probe
timeout, and `500` for unexpected internal errors.

## Outbounds

Preserve `SQStatsTrait` as a separate trait from `Outbound`. When constructing
the outbound list, clone each concrete QUIC outbound's `Arc` into a tag-keyed
`HashMap<String, Arc<dyn SQStatsTrait>>`, then coerce the same allocation into
`Arc<dyn Outbound>` for dispatch. Probe using the manager's existing outbound
list and `Outbound::handle`; do not add a probing trait or API methods to
`Outbound`. If DNS wraps an outbound, keep the
statistics map pointing to the original QUIC client; `ResolvingOutbound` does
not implement `SQStatsTrait`. Probes use the DNS wrapper from the dispatch list
so they use the configured resolver.

Use the manager's outbound list with DNS service outbounds removed for both
listing and probing. Each entry contains `tag` and a `quality` object:

- `sq_conn_stats`: the latest available cached `SQConnStats`, or `null` when
  unsupported, disconnected, or no sample exists. Expose `uplink` and `downlink`
  using the existing `ConnStats` fields (`lost_packets`, `sent_packets`, `rtt`,
  and `current_mtu`); `rtt` is in milliseconds and `current_mtu` is in bytes.
  Replace the internal `Instant` with `sample_age_ms`.
- `probe`: `null` before any completed probe, otherwise the latest probe object
  described below. Keep it until another probe completes; its timestamp lets
  clients determine freshness.

This endpoint reads cached state only. It must not connect an idle outbound,
request fresh peer statistics, or initiate a probe.

## Runtime selector controls

A `type: selector` outbound defines a nonempty, unique `outbounds` list of
configured outbound tags and optional `selected` member, defaulting to the first.
Resolve forward references and reject unknown members, invalid initial choices,
self-references, and cycles across all possible selector members before opening
listeners. Reuse existing outbound instances and their DNS wrappers. Selectors
remain available without the REST API feature.
Keep member controls in a separate `Selectable` trait, with no selection method
on `Outbound`. Register each selector's shared instance in
`manager.selectable_outbounds` as `Arc<dyn Selectable>` and pass that registry
to the API independently of the outbound dispatch map.

Expose the current state as `{"selected":"tag","outbounds":["tag","other"]}`
through `GET /api/v1/outbounds/{tag}/selection` and the `selector` field on selector
entries in the outbound list. Omit that field for other outbound types.
`PUT` on the same route accepts `{"selected":"other"}` and returns the updated
state. Reject invalid bodies and nonmembers with `400`, unknown outbound tags
with `404`, non-selector controls with `400`, and unsupported methods with `405`.
Apply existing authentication, path decoding, and JSON body limits.

Switch subsequent sessions atomically without moving or closing active TCP
sessions or UDP associations. Keep selection in memory and restore the configured
initial member on restart. Probes dispatch through the selected member; existing
probe caches retain their result until another probe completes. Connection
tracking reports the selector tag chosen by routing.

## Connections

Return active tracked sessions from every registered observer. Ignore entries
whose tracked halves have both expired, even if periodic cleanup has not run.
Each entry contains:

| Field | Meaning |
| --- | --- |
| `id` | Opaque session ID, stable and unique within this manager's lifetime |
| `protocol` | `tcp` or `udp` |
| `half_closed` | `true` when exactly one scoreboard weak pointer can be upgraded |
| `inbound` | Inbound tag |
| `outbound` | Selected outbound tag, or `null` when send stats have expired |
| `src` | Client socket address, or `null` when unavailable |
| `dst_ip` | Known destination IP, or `null` when only a domain is known |
| `dst_domain` | Routed destination domain or cached hostname, otherwise `null` |
| `dst_port` | Destination port, or `null` when send stats have expired |
| `upload_bytes` | Per-session upload bytes, or `null` when receive stats have expired |
| `download_bytes` | Per-session download bytes, or `null` when send stats have expired |
| `duration_ms` | Elapsed tracking duration in milliseconds, using `Instant`, or `null` when receive stats have expired |
| `first_response_ms` | Time to first response, or `null` |

IDs remain stable whichever half survives. Source addresses require receive
stats; destination addresses and outbound tags require send stats. Computing
`first_response_ms` requires both halves. Missing stats never remove completed
traffic from `user_stats`.

Report the destination after fake-IP restoration and router rewrites. If the
routed destination is a domain, report that domain. For an IP destination, use the shared
`DnsCache::reverse_lookup_cache` to obtain an optional hostname; this selects
the most recently cached matching name and ignores expired entries. Keep the
IP alongside the hostname. Do not perform DNS I/O or modify routing state while
serving this endpoint. Without `dns-server`, skip the cache lookup.

## Overall traffic

Return one entry for every registered observer, including idle observers:

```json
{
  "inbounds": [
    {"inbound": "proxy-in",
     "tcp_download_bytes": 2048, "tcp_upload_bytes": 1024,
     "udp_download_bytes": 512, "udp_upload_bytes": 256,
     "tcp_conns": 2, "udp_conns": 1}
  ]
}
```

Return TCP and UDP byte totals separately. Connection counts follow the lifetime of
`TrackedRecvStats`, rather than both halves. Sum the counters in all
`user_stats` entries; summing active scoreboard entries would lose completed
sessions' traffic.

## Memory usage

Use jemalloc allocator statistics. Return `allocator: "jemalloc"` and the byte
fields `allocated_bytes`, `active_bytes`, and `resident_bytes`, corresponding
to jemalloc's `stats.allocated`, `stats.active`, and `stats.resident` metrics.
Build jemalloc with statistics enabled and refresh the statistics epoch before
reading them, as specified in the [jemalloc manual](https://jemalloc.net/jemalloc.3.html).
These are allocator metrics; they do not represent total process RSS.

The independent `jemalloc` feature is enabled by default and installs the global
allocator in the library, covering both the CLI and library embeddings even
without `restful-api`. Keep allocator dependencies platform-gated and support
Linux and macOS on x86_64 and aarch64, plus Android on aarch64, armv7, i686,
and x86_64. Build 64-bit Android jemalloc with `JEMALLOC_SYS_WITH_LG_PAGE=14`
for compatibility with both 4 KB and 16 KB system pages. An embedding defining
its own global allocator must disable `jemalloc`.

Without `jemalloc`, do not compile the memory module or register the memory
endpoint; `/api/v1/memory` returns `404`. With both features, where statistics are
unavailable, keep the other endpoints usable and return `501` with error code
`memory_stats_unavailable`; never substitute zero values.

## Outbound probing

Accept a percent-encoded outbound tag as one path segment and a JSON body:

```json
{"url": "http://example.com/", "timeout_ms": 10000}
```

- Require an absolute `http://` URL with a host and no embedded credentials.
  HTTPS is outside this version's scope. Default `timeout_ms` to `10000` and
  accept values from `1` through `60000`.
- Send an HTTP GET through the named outbound directly, bypassing router
  selection. Preserve the URL's host, port, path, and query in the request.
  Dispatch a TCP session backed by an in-memory stream to `Outbound::handle`.
  Return `501` when the handler reports an unsupported operation; an outbound
  that closes the stream (such as `drop`) produces a failed probe with `502`.
- Measure monotonic elapsed time from the start of the outbound attempt until
  complete response headers arrive. Include destination resolution, connection
  setup, and request transmission; exclude JSON parsing and response-body
  download. Apply the timeout to the entire attempt.
- Treat `2xx` final responses as success. Do not follow redirects. Other final
  statuses, connection failures, and invalid responses are probe failures.
- Permit at most one running probe per outbound, and release all probe resources
  on success, failure, timeout, cancellation, or manager shutdown.
- Probes must not create client session entries or change API traffic totals.

A successful probe returns and caches:

```json
{"url": "http://example.com/", "status": "ok", "latency_ms": 42,
 "measured_at": "2026-10-17T08:00:00Z", "error": null}
```

A failed or timed-out probe replaces the previous cached probe with the same
fields, `status: "failed"` or `"timeout"`, `latency_ms: null`, and an `error`
object containing `code` and `message`. Return the corresponding HTTP error
with the common error envelope. Validation failures, unsupported operations,
concurrent-probe rejections, and cancelled probes do not overwrite the cache.

## CORS

- Include `Access-Control-Allow-Origin: *` on every response, including errors.
- Handle `OPTIONS` requests carrying `Origin` and `Access-Control-Request-Method`
  before authentication or route execution. Return an empty `204`, allowing
  `GET, POST, PUT, OPTIONS` and request headers `Authorization, Content-Type`.
- Continue to authenticate actual API requests. Do not enable cookie credentials.

## Acceptance criteria

- Verify feature gating, configuration validation, listener startup failure,
  and shutdown behavior.
- Verify bearer authentication on every route, rejected credentials before probe
  dispatch, omitted and empty secrets, and wildcard listeners with or without a secret.
- Verify unauthenticated CORS preflights and CORS headers on success and error responses.
- Verify authenticated and unauthenticated tracking across supported inbound
  types, with platform-gated TPROXY and TUN coverage.
- Cover partial and vectored TCP I/O, failed I/O, empty writes, first-response
  timing, UDP half lifetimes, both half-close directions, cancellation, and
  expired scoreboard entries.
- Verify totals include named and anonymous user counters, retain completed
  traffic, include restored traffic, follow user-stat resets and user removal,
  and exclude API probes.
- Cover destination ports, nullable addresses, cached domain hits and misses,
  and operation without `dns-server`.
- Verify response schemas, empty lists, error mappings, and cache-only reads.
- Use a local HTTP fixture and a controlled outbound for probe tests. Cover
  success, non-2xx responses, timeout, invalid input, concurrent requests, cache
  replacement, and resource cleanup without relying on public internet access.
- Verify the independent allocator feature, memory route absence without
  `jemalloc`, statistics on supported targets, and the `501` target fallback.
