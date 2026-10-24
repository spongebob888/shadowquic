# Transport stream and window limits

`shadowquic` and `sunnyquic` outbounds and inbounds accept five limits that
decide how much a connection may carry at once and how much it can be made to
buffer. All five are nonnegative integers, and the three windows are in bytes.

| key | outbound default | inbound default |
| --- | --- | --- |
| `max-concurrent-bidi-streams` | `500` | `1000` |
| `max-concurrent-uni-streams` | `500` | `1000` |
| `stream-receive-window` | `5000000` | `5000000` |
| `receive-window` | unset | unset |
| `send-window` | unset | `40000000` |

```yaml
outbounds:
  - tag: shadowquic-outbound
    type: shadowquic
    max-concurrent-bidi-streams: 2000
    stream-receive-window: 2000000
    receive-window: 32000000
```

`max-concurrent-bidi-streams` and `max-concurrent-uni-streams` are how many
streams of each kind the peer may hold open at once. Every proxied request and
every UDP association holds one bidirectional stream for its whole lifetime, so
the bidirectional value is also the ceiling on concurrent sessions over a single
connection; raise it to serve more at once. A peer that reaches the ceiling waits
for this side to free a stream, and the outbound side — which awaits its outbound
inline — stops accepting until one is freed. With `over-stream` on, this side's
UDP replies travel over unidirectional streams, which the other value bounds.

`stream-receive-window` is how many bytes the peer may have in flight on one
stream, and therefore the most this side can be made to buffer for it. The
default is a bandwidth-delay product of 400 Mbit/s over a 100 ms path, which
keeps flow control from becoming the bottleneck on long, fast links.

`receive-window` is how many bytes the peer may have in flight across every
stream at once. Left unset, the transport's own default stands, which is
unlimited — so the only bound on buffering is `max-concurrent-bidi-streams`
multiplied by `stream-receive-window`, which for the defaults is 1000 × 5 MB for
a single inbound connection. Setting it caps buffering independently of the
stream count, which is the lever to reach for on a memory-constrained
deployment.

`send-window` is how many bytes this side may have in flight before it stalls
waiting for acknowledgement. Left unset, the transport's own default stands.

These are properties of a connection, applied when it is established: a change
affects connections established afterwards, and a connection that is already
open keeps what it started with. Its stream ceiling is only ever raised while it
lives, never lowered, so lowering one of the stream counts cannot close streams
that are already open. None of them is a timeout, and none of them cuts off a
transfer that is still making progress — but a window below the path's
bandwidth-delay product does limit throughput. `0` is accepted and leaves that
direction unable to make progress.

## MTU black-hole detection

Both `shadowquic` outbounds (clients) and inbounds (servers) accept
`blackhole-threshold`, a nonnegative integer that defaults to `15`. A black hole
is detected when the number of suspicious packet loss bursts exceeds this
value. `0` triggers detection on the first suspicious burst. Higher values
reduce false positives on lossy networks but delay detection of actual MTU
reductions. Each endpoint applies its own threshold to its outgoing path.

```yaml
mtu-discovery: true
blackhole-detection: true
blackhole-threshold: 10
```

The threshold only applies when `mtu-discovery` is enabled (the default).
`blackhole-detection` controls whether detection resets the discovered MTU to
`min-mtu`. With that setting disabled (the default), detection keeps the current
MTU and still starts the cooldown before another MTU discovery run.
