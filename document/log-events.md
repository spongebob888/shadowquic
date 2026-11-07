# Log events

Every log emitted by the Shadowquic library and CLI carries a structured string
field named `event`. Values use `category:event_name`, for example:

```text
event="connectivity:connection_started"
event="security:authentication_succeeded"
event="proxy:tcp_request_accepted"
event="routing:outbound_selected"
event="squic:bidirectional_stream_opened"
event="proxy:tcp_request_completed"
```

This naming convention is inspired by [qlog's category and event names](https://www.ietf.org/archive/id/draft-ietf-quic-qlog-quic-events-03.html#section-3).
These are Shadowquic application events, not a complete qlog document or a claim
of conformance to a qlog event schema. The CLI continues to emit text logs;
applications embedding the library can capture the field with a `tracing`
subscriber. Dependency logs, test diagnostics, and example/performance program
logs are outside this event contract.

## Parsing a connection timeline

Use the `event` field to classify each record, its timestamp to order it, and
its existing span and event fields to associate it with a connection or request.
Keep the entire span hierarchy: `inbound` includes its tag and, where available,
source, user and connection `id`; `bistream` has a stream `id`; request spans
include source and destination; UDP records may include `context_id`. IDs at
different levels have different scopes. Event names identify kinds of activity,
not individual connections. Some records are global configuration or lifecycle
events and do not belong to a connection. Existing logs do not provide a unique
connection ID on every record, so address-only correlation can be ambiguous.

Parse the explicit field, not the human-readable message or a substring inside
it. Messages can change independently of event names. Respect the configured
log level: debug/trace events will be absent at info level. An accepted inbound
connection and an established outbound connection represent different milestones;
a UDP association or stream closing does not imply the underlying QUIC connection
has closed. `connectivity:connection_close_requested` records an observer's close
request, whereas `connectivity:connection_closed` records a reported closure.

## Categories

| Category | Activity |
| --- | --- |
| `connectivity` | Connection attempts, acceptance, establishment, closure, sockets, paths |
| `security` | Authentication, handshakes, certificates, user administration |
| `squic` | SQ streams, datagrams, UDP associations and context lifecycle |
| `direct`, `socks`, `shadowquic`, `sunnyquic`, `tproxy`, `tun` | Transport activity for the corresponding inbound/outbound type |
| `recovery` | Congestion controllers and connection metrics |
| `proxy` | Accepted/sent/completed requests, request failures, inbound lifecycle, shared UDP queue drops |
| `routing` | Outbound selection, routing completion, router script logs and reloads |
| `dns` | Queries, responses, cache hits, address resolution, fake-IP mappings |
| `sniffer` | Protocol sniffing outcomes |
| `application` | Startup, working directory and shutdown |
| `configuration` | Configuration adjustments and unsupported updates |
| `api`, `dashboard` | API connections and dashboard preparation |
| `database`, `dhcp` | Routing database and DHCP lease maintenance |

For new logging calls, provide a literal `event = "category:snake_case_name"`
field. For transport activity, use the inbound/outbound type as the category
(for example, `socks:udp_association_established` or
`sunnyquic:datagram_receive_failed`); SQ protocol internals use `squic`.
Shared proxy code without a concrete inbound/outbound type uses `proxy`.
Reuse event names for the same semantic activity within a category. Keep names
stable when rewriting messages. Use distinct names for attempts, successes and
failures.
