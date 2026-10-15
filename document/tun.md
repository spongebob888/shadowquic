# TUN inbound

The `tun` inbound receives IPv4 and IPv6 TCP/UDP packets through a layer-3 TUN
adapter, using [tun-rs](https://docs.rs/tun-rs/2/tun_rs/) for device I/O and
[smoltcp](https://docs.rs/smoltcp/0.12/smoltcp/) for the TCP stack and IP/UDP packet
handling. It is enabled by the opt-in `tun` Cargo feature and available on Linux,
macOS, and Windows. Build with `cargo build --features tun`. Creating an adapter requires the
platform's network administration permissions. Windows also needs the Wintun
runtime required by tun-rs.

```yaml
inbounds:
  - tag: tun-in
    type: tun
    # name: sq-tun0
    ipv4: 172.19.0.1/30
    ipv6: fdfe:dcba:9876::1/126
    mtu: 1500
    max-sessions: 1024
    idle-timeout: 300
    default-outbound: proxy
```

`tag` is required. `name` is optional; the OS chooses a name when omitted. On
macOS, use `utunN` or leave it unset. `ipv4` and `ipv6` are interface addresses
with CIDR prefixes, not fake-IP pools. They default to the values shown above.
The device is brought up when the inbound is created and closed on shutdown.

`mtu` defaults to 1500 and must be at least 1280. `max-sessions` defaults to 1024
and bounds the combined TCP connections and UDP client sessions (1–65,536).
`idle-timeout` defaults to 300 seconds (1–86,400). Queues and TCP buffers are
bounded. At capacity, new flows are dropped; congested UDP queues drop packets
without blocking other flows. Idle or closed UDP sessions and closed TCP sockets
release their capacity. TCP preserves half-closes. UDP sessions are grouped by
client socket address and retain the destination of every packet.

Requests carry `tag`, `default-outbound`, the client's source address, and the
original destination into the normal router and outbound path. `default-outbound`
is optional and behaves like that setting on other inbounds. Outbound protocols
can carry TUN requests without special configuration.

## Fake IP and DNS interception

With `dns-server` enabled and a `dns-fakeip` service configured, TUN automatically
uses the same mapping as TPROXY. Fake TCP destinations and the initial UDP
destination are restored to domain names **before routing**. Every later UDP
packet is restored as well, and replies use the application's original fake
source IP and port. Unmapped addresses inside the configured fake-IP pools are
rejected. Ordinary IP destinations pass through unchanged. Fake-IP eviction and
index reuse follow the rules in [DNS services](dns.md).

Route TUN DNS traffic to the DNS service's tag to intercept it. The
[complete example](../shadowquic/config_examples/tun.yaml) includes a fake-IP
service and a port-53 routing rule. Route the DNS service's own upstream sessions
to a transport outbound when using an upstream resolver.

## OS routing

The inbound configures the adapter's addresses and MTU. Configure routes, DNS
settings, forwarding, and exclusions in the operating system. It does not install
a default route or change the system resolver. Keep proxy server addresses and
upstream transport traffic outside the TUN routes to avoid interception loops.

For a Linux adapter explicitly named `sq-tun0`, fake-IP-only capture can use:

```sh
sudo ip route add 198.18.0.0/15 dev sq-tun0
sudo ip -6 route add 2001:2::/48 dev sq-tun0
```

Use your configured fake-IP ranges. Point clients at the configured DNS listener,
or route their DNS traffic into TUN and intercept it with a routing rule. Capture
of other destinations needs corresponding routes and upstream exclusions.

## Packet support and tests

The inbound accepts unfragmented IPv4/IPv6 TCP and UDP packets. IP fragments,
IPv6 extension headers, ICMP, and other protocols are dropped. UDP replies larger
than the configured MTU fail rather than fragmenting; choose an MTU suitable for
your traffic. TCP segmentation follows the device MTU.

Packet-level tests exercise real TCP handshakes, data, half-closes, UDP replies,
custom fake-IP pools, and routing metadata without creating a host interface:

```sh
cargo test -p shadowquic --features tun --lib tun::
```

A device creation/shutdown smoke test is ignored by default because it requires
network administration permissions. Run it in a suitable isolated environment:

```sh
cargo test -p shadowquic --features tun --lib tun::tests::kernel_tun_device_can_start_and_shutdown -- --ignored
```

The Docker integration test builds a static Linux binary with `tun`, `dns-server`,
and Lua routing enabled, without `tproxy`. It runs real kernel TCP/UDP traffic
through a TUN adapter in an isolated container, including IPv4/IPv6, intercepted
DNS, fake-IP restoration, multiple UDP destinations, TCP half-closes, and device
cleanup on shutdown:

```sh
python3 scripts/test_suite/test_tun.py
```

The runner needs Docker, `/dev/net/tun`, and a Rust musl target and linker. Use
`nix develop` for the linker environment. The container receives `NET_ADMIN` and
the TUN device; its test routes and firewall rules are confined to that container.
To reuse a static Linux binary already built with these features, set
`SHADOWQUIC_TUN_TEST_BINARY` to its host path. The runner also participates in
`scripts/main_test.py`, alongside the TPROXY integration test.
