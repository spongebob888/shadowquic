# Hostname sniffing

Build with `cargo build --release --features sniffer`. The opt-in `sniffer`
feature enables `plugin`; use `plugin-vendored` (already in the default features)
or your platform's Lua library. QUIC inspection requires the `ring` or
`aws-lc-rs` feature, or an installed rustls-jls provider with AES-128-GCM support.

Configure hostname inspection globally under `router.sniffer`. A Lua script
(`router.src` or `router.path`) is required. Omitting `sniffer` disables inspection;
omitting `tcp` or `udp` disables that transport. Configuration is rejected when
the binary was built without `sniffer`.

```yaml
router:
  sniffer:
    tcp:
      - ports: [80, 8080]
        protocols: [http]
      - ports: [443, 8443]
        protocols: [tls]
    udp:
      - ports: [443]
        protocols: [quic]
    timeout: 100                 # milliseconds, absolute deadline per request
    max-bytes: 32768
    max-concurrent: 256
    max-buffered-bytes: 8388608
  src: |
    return function(ctx)
      if ctx.sniffer and ctx.sniffer.domain == "blocked.example" then
        return "blocked"
      end
      return "direct"
    end
```

Ports are destination ports; `ports: []` (or omitted ports) matches every port.
Matching rules combine their protocol sets. TCP supports `http` and `tls`; UDP
supports `quic`. Empty protocol lists and transport/protocol mismatches are errors.
Matching protocols share one inspection deadline and reservation.

Inspection runs in each request's task after inbound negotiation and fake-IP
restoration, before Lua routing. It applies to all requests using that router,
including internal requests. Calling `Inbound::accept` directly does not sniff.
No inspection buffers or timers are created for unmatched requests.

`UserContext::sniffer` and `RouteContext::sniffer` contain the same optional
`SniffResult`. Lua exposes a read-only `ctx.sniffer` userdata, or `nil` if no valid
hostname was obtained. Its fields are:

| Field | Values |
| --- | --- |
| `domain` | Lowercase ASCII hostname; HTTP ports and one trailing dot are removed; IP literals are not hostnames |
| `type` | `"http"`, `"tls"`, or `"quic"` |
| `version` | String: HTTP `"1.0"` / `"1.1"`, TLS `"1.0"`–`"1.3"`, QUIC `"1"` / `"2"` |

TLS version means the highest recognized version advertised in ClientHello, not
a negotiated server version. No metadata is published without a valid hostname.
Sniffing never changes the destination; rewriting remains an explicit router
operation. Client-provided hostnames are routing hints, not authenticated service
identities. ECH only exposes the outer ClientHello name.

HTTP inspection supports HTTP/1.0 and HTTP/1.1 request headers. Only `Host` is
used, regardless of the request-target form. Missing, duplicate, malformed, or
folded Host headers produce no result. Cleartext HTTP/2 is not inspected.
TLS inspection handles the initial ClientHello, including fragmented reads and
handshakes spanning records. STARTTLS and later handshakes are not inspected.

QUIC v1 and v2 inspection checks the header before deriving Initial keys and
decrypting. It inspects the destination of the first datagram, collecting at most
five datagrams in arrival order under one absolute timeout. ClientHello CRYPTO
data and packet numbers are retained across datagrams and coalesced Initial
packets. If a datagram for another destination arrives before inspection finishes,
inspection aborts without a hostname. All consumed datagrams, including the one
for another destination, are replayed in their original order and with their
original destinations. Later traffic is not sniffed again.

`max-bytes` bounds the aggregate inspected wire bytes and reassembled handshake.
Unsupported versions, invalid authentication, incomplete handshakes, conflicting
CRYPTO overlaps, or unsupported frames produce no result. At most 32 Initial
packets total and 4096 frames per packet are processed. Missing packets or a
handshake requiring more than five datagrams can prevent detection.

Timeout, malformed input, missing hostname, size limits, and exhausted budgets
fall back to routing with `ctx.sniffer == nil`. Stream/receiver I/O errors close
the request. TCP bytes and UDP datagrams are replayed unchanged, including bytes
read past a header. Server-first TCP protocols may wait until the timeout before
the outbound connects, so restrict ports where possible.

Limits are global per configured router: `timeout` 1–60000 ms, `max-bytes`
256–1048576, `max-concurrent` 1–65536, and `max-buffered-bytes` from four times
`max-bytes` (plus 65536 when UDP is configured) through 268435456. Each inspection reserves four times `max-bytes`
for retained wire data, decryption, and reassembly storage. UDP reserves an extra
65536 bytes for the final datagram that can cross the byte limit. The reservation stays
held until the replay buffer is drained or dropped, including while waiting for
an outbound. Default limits allow 64 simultaneous TCP reservations or 42 UDP reservations. Budgets cover
inspection buffers, excluding allocator overhead and existing transport buffers.

Enable `RUST_LOG=shadowquic::utils::sniffer=debug` to log each admitted inspection's
result and `elapsed` duration (with units), including waiting for input. Successful
hostname extraction logs the domain, protocol, and version; unsuccessful
inspection logs `result=None`, and I/O failures log the error. Requests skipped
by port rules or exhausted budgets do not produce an inspection log.

Run regression tests with:

```sh
cargo test --release -p shadowquic --features sniffer --lib sniffer
```
