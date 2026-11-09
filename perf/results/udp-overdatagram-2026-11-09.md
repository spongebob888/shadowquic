# UDP over QUIC DATAGRAM throughput investigation — 2026-11-09

The main loss bottleneck at the default offered load is the server's **two
32-packet association channels**, combined with nonblocking `try_send` delivery.
In a million-packet run, full-channel drops explained **312,038 of 321,434 missing
packets (97.1%)**. A temporary 1,024-slot control almost eliminated those drops.
Under unpaced overload, **QUIC send-buffer eviction** becomes the dominant loss
source instead. The production queue sizes and transport code were restored
after these controls; retained changes are confined to the perf harness and docs.

## Setup and interpretation

- Revision: `d599510` plus the perf harness changes in this working tree.
- Local Linux 6.12.113, AMD Ryzen 9 9900X3D, 24 logical CPUs; rustc 1.99.1.
- Optimized release: opt-level 3, fat LTO, one codegen unit, symbols retained.
- One process runs traffic generators, SOCKS5 inbound, ShadowQUIC client/server,
  and DirectOut. QUIC uses loopback, BBR, UDP over DATAGRAM, 1,000-byte payloads,
  and an eight-worker Tokio runtime. Upload and download run simultaneously.
- A warmup round trip precedes timing. Most runs send 100,000 packets/direction;
  the two long controls below send 1,000,000. Each receiver drains for up to
  500 ms after its sender finishes. That drain is excluded from receive rates.
- Rates are payload MiB/s, measured from first to last unique received packet;
  duplicates do not count. High-rate results with substantial loss are not
  lossless capacity figures. A short warmup does not establish steady-state BBR.
- The requested 1 µs pacing sleep is approximately a millisecond in practice.
  Batch size changes both offered rate and burst size, so this is not a
  constant-rate network capacity test. Host scheduling and startup affect results.
- Kernel counters were sampled through `/proc/net/snmp`, with per-process socket
  inode/queue/drop snapshots from `/proc/PID/net/udp{,6}`. SNMP deltas are host-wide.

## Controlled result at the default offered load

Both long runs used `PERF_UDP_PACKETS=1000000`, batch 100, requested pause 1 µs.
The 32-slot run was CPU-profiled; the 1,024-slot control was unprofiled, so exact
speed differences include sampling and run-to-run variation. The drop counts and
separate repeated short controls provide the causal evidence.

| Server association channels | Offered up/down MiB/s | Delivered up/down MiB/s | Upload/download loss | Full-channel drops |
| --- | --- | --- | --- | --- |
| Production, 32 slots | 80.26 / 80.89 | **63.05 / 72.23** | **21.44% / 10.70%** | **312,038** |
| Temporary control, 1,024 slots | 81.29 / 82.20 | **80.52 / 81.98** | **0.95% / 0.26%** | **85** |

The 1,024-slot long run delivered 990,467 upload and 997,371 download packets.
Its kernel receive-buffer error delta was 3,424. Three shorter 1,024-slot runs
also reproduced lower channel loss: 0–236 channel drops per 200,000 offered
packets, versus 29,284 in the representative 32-slot short run.

The relevant code is in
[`squic/inbound.rs`](../../shadowquic/src/squic/inbound.rs), which creates two
`channel::<(Bytes, SocksAddr)>(32)` queues for each UDP association, and
[`lib.rs`](../../shadowquic/src/lib.rs), whose `UdpSend for Sender` calls
`try_send`. A full channel emits `proxy:datagram_dropped` and returns `Ok(0)`.
[`IDStore::feed_datagram`](../../shadowquic/src/squic/mod.rs) treats that as a
successful send. The receiver can therefore drain QUIC bursts faster than the
association task drains its small queue, discarding payload without propagating
backpressure. The low system CPU utilization does not make these bursts safe.

## Offered-load sweep with production queues

These are individual 100,000-packet runs with independent traffic tasks and the
bounded SOCKS receive buffer. Read the loss alongside the rate.

| Batch | Offered up/down MiB/s | Delivered up/down MiB/s | Upload/download loss | Channel drops | Kernel receive-buffer errors |
| --- | --- | --- | --- | --- | --- |
| 25 | 22.06 / 22.15 | 22.04 / 22.15 | 0.09% / 0.003% | 97 | 0 |
| 50 | 41.57 / 41.91 | 37.71 / 40.51 | 9.29% / 3.34% | 11,452 | 7 |
| 100 | 82.79 / 83.19 | 60.04 / 75.08 | 27.48% / 9.74% | 29,284 | 220 |
| 200 | 163.16 / 167.52 | 103.61 / 134.85 | 36.49% / 19.51% | 37,752 | 3,156 |
| 500 | 447.34 / 457.47 | 183.97 / 212.56 | 58.82% / 50.59% | 8,200 | 5,562 |
| Unpaced | 510.85 / 532.23 | 99.69 / 123.15 | 80.12% / 71.55% | 5,586 | 3,583 |

The direct SOCKS→DirectOut baseline at batch 100 delivered 76.11 / 77.57 MiB/s,
with 5.44% / 4.20% loss and 9,638 kernel receive-buffer errors. At batch 50 it
delivered 42.49 / 42.68 MiB/s, with 0.09% / 0.12% loss. Its losses were accounted
for by kernel socket receive buffers; it has no QUIC association channels.
The host's UDP receive-buffer default and maximum were both 212,992 bytes.

Three unprofiled final-harness repeats at batch 100 delivered 61.35–66.38 MiB/s
upload and 74.51–76.79 MiB/s download, with 21.33–25.57% / 8.66–9.63% loss.
Replacing timer sleeps with scheduler yields at batches 4–64 still offered
412–714 MiB/s and lost 61–81% of traffic, so yielding the load generator alone
does not solve overload. SunnyQUIC also passed a 1,000-packet/direction smoke run
with zero loss through the shared harness.

## Why unpaced overload loses packets

A temporary tracing counter counted Quinn DATAGRAM eviction events, with the
server association queues at 1,024 slots. It did not print per-packet logs.
Enabling Quinn trace events adds instrumentation cost; use these runs for loss
attribution rather than absolute performance comparison.

| Workload | Missing payload packets | QUIC outgoing evictions | QUIC incoming evictions | Channel drops | Kernel receive-buffer errors |
| --- | --- | --- | --- | --- | --- |
| Batch 100, 100k/direction | 6,599 | 6,257 | 0 | 0 | 342 |
| Unpaced, 100k/direction | 163,500 | 160,477 | 0 | 0 | 3,023 |

In both controls the counted loss sources exactly sum to missing payload packets.
In the unpaced control, send-buffer eviction accounted for **98.2%** of loss.
The application submitted roughly 649 / 668 MiB/s while delivering only
89 / 117 MiB/s. Increasing association queues does not fix that overload.

The wrapper in
[`quinn_wrapper/wrapper.rs`](../../shadowquic/src/shadowquic/quinn_wrapper/wrapper.rs)
uses immediate `Connection::send_datagram`, which permits eviction of older
unsent datagrams. The client retains Quinn's default 1 MiB outgoing DATAGRAM
buffer; the server sets 10,000,000 bytes. Quinn's local dependency source confirms
that `send_datagram_wait` instead waits for buffer space. A single handshake
warmup does not remove congestion-controller startup or queue pressure.

## Benchmark artifacts removed

The original `udp_direct` executable failed in SOCKS parsing because its manual
accept path did not set the request task-local context. It now uses the normal
manager and shares measurement and packet validation with the QUIC benchmarks.

The original senders and receivers were branches of the same task. Under
unpaced simultaneous traffic, the original layout delivered only 121/100,000
upload packets through ShadowQUIC and 93/100,000 through DirectOut in one sweep.
Independent sender/receiver tasks remove that starvation artifact. They do not
remove genuine UDP overload loss.

`fast-socks5::Socks5Datagram::recv_from` embeds a 64 KiB scratch buffer in its
async future. Moving that future through the measurement `select!` introduced
large copies for each 1,000-byte packet. A payload-sized buffer plus the maximum
SOCKS header retains parsing/fragment checks and reduces this artifact:

| User CPU self samples | Original harness | Independent tasks, 64 KiB buffer | Bounded receive buffer |
| --- | --- | --- | --- |
| `memmove` | 27.40% | 29.28% | 6.86% |
| `memset` | 3.03% | 3.72% | 0.79% |

The original and bounded profiles used one million packets/direction and
6,930 / 8,819 samples; the middle control used 300,000 packets and 2,097 samples.
These are shares of user CPU samples, not end-to-end time. The remaining hot
symbols include Tokio readiness/scheduling, connection mutex contention, QUIC
socket polling, protocol processing, and AES-GCM. Kernel time is excluded from
the sample percentages. There is no evidence here that encryption is the main
cause of the default-load delivery loss.

## Reproduction and retained artifacts

See [`perf/README.md`](../README.md) for environment controls and profiling
commands. Build before starting timed runs, run benchmarks sequentially, and
leave `RUST_LOG` unset. For the main comparison:

```sh
CARGO_PROFILE_RELEASE_STRIP=false cargo build -p perf --release \
  --bin udp_shadowquic --bin udp_direct
PERF_UDP_PACKETS=1000000 target/release/udp_shadowquic
PERF_UDP_BATCH=25 target/release/udp_shadowquic
PERF_UDP_BATCH=0 target/release/udp_shadowquic
PERF_UDP_BATCH=100 target/release/udp_direct
```

The queue control changed only the two server association channel capacities
from 32 to 1,024, rebuilt, repeated the runs, then restored the original source.
The temporary Quinn counter enabled `::connection::datagrams` trace events and
counted messages `dropping outgoing datagram` and `dropping stale datagram`.
That additional instrumentation was also removed.

The structured sweep data is in
[`udp-overdatagram-2026-11-09.csv`](udp-overdatagram-2026-11-09.csv). Kernel/channel
counters and process user/system times are per-run totals repeated in each
up/down row; do not sum those columns across directions. Raw profiles, logs,
JSON socket snapshots, the collection script, and temporary source snapshots
are retained locally under `/tmp/shadowquic-udp-profile/`.

The next production change to evaluate is association queue admission and drain
scheduling: burst-aware capacity, explicit bounded backpressure, or fairer
batching. Under overload, evaluate waiting on QUIC send capacity together with
upstream UDP drops and latency. Larger buffers alone move the loss boundary and
consume memory; the controls here establish cause, not a universal queue size
or a maximum lossless network throughput.

## Validation

- Release builds of `udp_shadowquic`, `udp_direct`, and `udp_sunnyquic` passed.
- `cargo test -p perf --release --lib`: 7 tests passed, including bounded SOCKS
  header/length validation and drop-counter regression coverage.
- `cargo clippy -p perf --release --all-targets`, `cargo fmt --all -- --check`,
  and `git diff --check` passed.
- CLI smoke checks passed for upload/download isolation, unpaced/yield modes,
  invalid direction, and zero-packet rejection. Direct and SunnyQUIC smoke runs
  delivered all 1,000 packets in both directions.
- Both production source files used for temporary controls compare byte-for-byte
  with their original snapshots. All retained modifications are under `perf/`.
