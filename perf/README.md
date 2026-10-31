Run the direct forwarding baselines from the repository root:

```sh
cargo run -p perf --release --bin tcp_direct
cargo run -p perf --release --bin udp_direct
```

Both use a local SOCKS5 client, SOCKS5 request parsing, and `DirectOut` to reach a
local peer. They use OS-assigned loopback ports and need no external server or
certificates. TCP transfers 1000 MiB in each direction, download then upload.
UDP sends 100,000 packets of 1000 bytes in each direction concurrently, with a
short pacing sleep after each batch of 100. Rates are payload MiB/s.

UDP starts both receivers before bulk traffic, reports received packets and
loss, and measures from the first to last received packet. A 500 ms receive
inactivity timeout drains late packets but is excluded from the reported rate.
The result is throughput at this offered load, rather than a saturation test.

The QUIC UDP benchmarks use a shared measurement harness:

```sh
cargo run -p perf --release --bin udp_shadowquic
cargo run -p perf --release --bin udp_sunnyquic
```

Run these sequentially from the repository root; they listen on SOCKS port 1089
and QUIC port 4444. SunnyQUIC uses the development certificates in `assets/certs`.
Each benchmark completes a warmup round trip before sending 100,000 packets of
1000 bytes per direction, with upload and download running concurrently.
The pacing is the same short sleep after each batch of 100 packets.

Each direction reports offered send rate, delivered receive rate, unique packets
received, packet loss, and duplicates. Receive timing spans the first to last
unique packet, excluding setup and the 500 ms drain after its sender finishes.
The receiver remains active until all packets arrive or that drain expires,
including across gaps while the sender is still running. Empty or single-packet
results report zero receive rate because they have no measurable time span.

Proxy tracing is off by default to keep per-packet logging out of the measurement.
Set `RUST_LOG=shadowquic=debug` when investigating forwarding behavior; logging
changes the measured performance. Packet loss is reported rather than hidden by
changes to the proxy's queue sizes or forwarding behavior.
