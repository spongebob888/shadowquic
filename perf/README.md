Run the forwarding benchmarks from the repository root:

```sh
cargo run -p perf --release --bin tcp_direct
cargo run -p perf --release --bin udp_direct
cargo run -p perf --release --bin udp_shadowquic
cargo run -p perf --release --bin udp_sunnyquic
```

TCP transfers 1000 MiB in each direction, download then upload. Its direct
baseline uses OS-assigned loopback ports. All UDP benchmarks use the same
SOCKS5 measurement harness and the normal proxy manager. Run UDP benchmarks
sequentially: they listen on SOCKS port 1089; QUIC also uses port 4444.
SunnyQUIC uses the development certificates in `assets/certs`.

UDP completes a warmup round trip before sending 100,000 packets of 1000 bytes
per direction, with upload and download running concurrently. By default each
sender sleeps for one microsecond after each batch of 100 packets. Tokio timer
resolution and scheduling make the actual pause longer, so this measures a
particular offered load rather than maximum throughput.

Control the offered load and isolate directions with environment variables:

- `PERF_UDP_PACKETS`: packets per direction, default `100000`, must be positive.
- `PERF_UDP_BATCH`: packets between pauses, default `100`; `0` disables pacing.
- `PERF_UDP_PAUSE_US`: requested pause in microseconds, default `1`; `0` yields
  to the scheduler without a timer when `PERF_UDP_BATCH` is nonzero.
- `PERF_UDP_DIRECTION`: `both` (default), `upload`, or `download`.

For example, build once and sweep the offered load without build interference:

```sh
cargo build -p perf --release --bin udp_shadowquic --bin udp_direct
for batch in 25 50 100 200 500 0; do
  PERF_UDP_BATCH=$batch target/release/udp_shadowquic
done
PERF_UDP_DIRECTION=upload PERF_UDP_BATCH=0 target/release/udp_shadowquic
```

Each direction reports offered send rate, delivered receive rate, unique packets
received, loss, and duplicates. Rates are payload MiB/s. Receive timing spans the
first to last unique packet, excluding setup and the 500 ms drain after its
sender finishes. Empty or single-packet results have zero receive rate. With
heavy loss, a short receive burst can have a high rate despite poor total delivery;
always read the rate together with packet counts and loss. The entire benchmark
has a 30-second timeout.

Senders and receivers run as independent tasks so a continuously ready sender
cannot prevent its receiver from being polled. The SOCKS receiver uses a buffer
sized for the benchmark payload and maximum SOCKS header, avoiding the 64 KiB
async receive buffer in `fast-socks5`. Both paths retain payload and header
validation. A separate tracing layer counts `proxy:datagram_dropped` events and
reports `proxy full-channel drops` without formatting per-packet log lines. This
counter covers the proxy's full association channels, not kernel socket drops or
QUIC send/receive queue eviction.

Proxy tracing is off by default. Set `RUST_LOG=shadowquic=debug` when investigating
forwarding behavior; logging changes the measured performance. The harness does
not change proxy queue sizes or forwarding behavior.

To sample user CPU stacks with release optimization and symbols:

```sh
CARGO_PROFILE_RELEASE_STRIP=false cargo build -p perf --release --bin udp_shadowquic
perf record --user-callchains -F 499 -e cycles:u --call-graph dwarf,16384 \
  -o /tmp/udp.data -- env PERF_UDP_PACKETS=1000000 target/release/udp_shadowquic
perf report -i /tmp/udp.data --stdio
```

`--user-callchains` permits user-space stack sampling on systems that disallow
kernel profiling. Samples exclude kernel CPU time; record process user/system
time and UDP socket drops separately when diagnosing network overhead.

Measured results and bottleneck attribution: [2026-11-09 UDP DATAGRAM investigation](results/udp-overdatagram-2026-11-09.md).
