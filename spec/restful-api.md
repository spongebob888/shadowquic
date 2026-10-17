# REST API

## Scope and configuration

Implement an optional HTTP API for outbound quality, active proxy sessions,
traffic counters, memory statistics, and outbound probing.

- Gate the API and its dependencies behind a non-default `restful-api` Cargo
  feature. Enabling it must automatically enable `statistics`.
- Start the API only when `api-config` is present. Reject that configuration
  when the binary was built without `restful-api`.
- Require `bind-addr`. The initial API supports loopback addresses only and
  has no authentication; reject non-loopback addresses during validation.
  Authenticated remote access is outside this version's scope.
- Run the API alongside the proxy listeners. A bind failure must fail startup;
  shut down the API and cancel probes when the manager shuts down.

```yaml
api-config:
  bind-addr: "127.0.0.1:9090"
```

## Observer coverage and metric semantics

Preserve observer support for ShadowQUIC and SunnyQUIC, and add support for
mixed, TPROXY, and TUN inbounds. Include standalone SOCKS inbounds through the
same request tracking path. Preserve existing feature and platform gates.

Register observers with `ApiManager` by inbound tag. Requests without an
authenticated user must still be tracked; API tracking must not require a
synthetic username or a QUIC connection handle. Preserve existing authenticated
user statistics and connection control behavior.

Internal DNS upstream and database download queues are excluded from client
tracking so probes and background work do not inflate proxy-session metrics.

In this API, a connection means one TCP proxy session or UDP association, not
the underlying QUIC transport connection. A UDP association can carry packets
for multiple destinations; its reported destination is the initial routed
destination. Do not create duplicate entries for cloned UDP send handles.

- All byte counters count successfully transferred proxy payload bytes, excluding
  protocol framing. Partial I/O counts only the bytes actually transferred;
  errors and pending operations do not increase counters.
- `recv_bytes` means bytes received from the inbound client (upload).
  `sent_bytes` means bytes sent back to the inbound client (download).
- Create session metadata when tracking starts, after routing selects the
  outbound. Thus creation time excludes inbound handshakes and router execution.
- `first_response_ms` is the monotonic duration from tracking creation to the
  first successful, nonempty write or datagram sent back to the client. It is
  `null` until that event and is set only once. TCP vectored writes count too.
- API traffic totals are counters for the lifetime of each observer. They start
  at zero and are independent of persisted user totals, user-stat resets, and
  user removal. Completed sessions remain included in byte totals.
- A TCP session is active while its tracked stream exists. A UDP association
  remains active while either tracked half exists. Keep its metadata and final
  per-half counters available until both halves are dropped.
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
| `get_mem_usage` | `GET /api/v1/memory` | Memory object |
| `probe_outbound` | `POST /api/v1/outbounds/{tag}/probe` | Probe object |

Successful requests return `200`. Empty collections return an empty array.
Sort outbound and traffic entries by tag, and connections by ID.

Errors use `{"error": {"code": "invalid_request", "message": "..."}}`.
Return `400` for invalid input, `404` for unknown routes or outbound tags, `405`
for unsupported methods, `409` for an already-running probe on the same outbound,
`501` for an unsupported operation, `502` for a failed probe, `504` for a probe
timeout, and `500` for unexpected internal errors.

## Outbounds

Return every registered outbound tag, including automatically registered DNS
outbounds. Each entry contains `tag` and a `quality` object:

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

## Connections

Return active tracked sessions from every registered observer. Ignore entries
whose tracked halves have both expired, even if periodic cleanup has not run.
Each entry contains:

| Field | Meaning |
| --- | --- |
| `id` | Opaque session ID, stable and unique within this manager's lifetime |
| `protocol` | `tcp` or `udp` |
| `inbound` | Inbound tag |
| `outbound` | Selected outbound tag |
| `src` | Client socket address, or `null` when unavailable |
| `dst_ip` | Known destination IP, or `null` when only a domain is known |
| `dst_domain` | Routed destination domain or cached hostname, otherwise `null` |
| `dst_port` | Destination port |
| `recv_bytes` | Per-session upload bytes |
| `sent_bytes` | Per-session download bytes |
| `created_at` | Time tracking began |
| `first_response_ms` | Time to first response, or `null` |

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
    {"inbound": "proxy-in", "sent_bytes": 2048, "recv_bytes": 1024,
     "tcp_conns": 2, "udp_conns": 1}
  ]
}
```

Byte totals combine TCP and UDP. Connection counts describe currently active
proxy sessions, using the lifetime rules above. Obtain these values from
observer counters; summing active scoreboard entries would lose completed
sessions' traffic.

## Memory usage

Use jemalloc allocator statistics. Return `allocator: "jemalloc"` and the byte
fields `allocated_bytes`, `active_bytes`, and `resident_bytes`, corresponding
to jemalloc's `stats.allocated`, `stats.active`, and `stats.resident` metrics.
Build jemalloc with statistics enabled and refresh the statistics epoch before
reading them, as specified in the [jemalloc manual](https://jemalloc.net/jemalloc.3.html).
These are allocator metrics; they do not represent total process RSS.

The CLI must use jemalloc as its global allocator on supported targets when
`restful-api` is enabled. Keep allocator dependencies platform-gated and
support Linux and macOS on x86_64 and aarch64. A library embedding
must opt into that allocator explicitly. Where jemalloc statistics are
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
  Reject outbounds that cannot carry HTTP-over-TCP probes with `501`.
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

## Acceptance criteria

- Verify feature gating, configuration validation, listener startup failure,
  and shutdown behavior.
- Verify authenticated and unauthenticated tracking across supported inbound
  types, with platform-gated TPROXY and TUN coverage.
- Cover partial and vectored TCP I/O, failed I/O, empty writes, first-response
  timing, UDP half lifetimes, cancellation, and expired scoreboard entries.
- Verify observer totals survive session completion, user-stat resets, and user
  removal, and exclude restored historical traffic and API probes.
- Cover destination ports, nullable addresses, cached domain hits and misses,
  and operation without `dns-server`.
- Verify response schemas, empty lists, error mappings, and cache-only reads.
- Use a local HTTP fixture and a controlled outbound for probe tests. Cover
  success, non-2xx responses, timeout, invalid input, concurrent requests, cache
  replacement, and resource cleanup without relying on public internet access.
- Verify jemalloc statistics on supported targets and the `501` fallback.
