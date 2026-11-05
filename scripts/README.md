## Standalone DNS 0-RTT probe

Run independently of the Rust workspace and the Python integration suite:

```sh
uv run scripts/dns_0rtt_probe.py --all-addresses \
  --json /tmp/dns-0rtt.json --qlog-dir /tmp/dns-0rtt-qlog
```

The script tests Google DNS (`dns.google`, `8.8.8.8` / `8.8.4.4`) and AliDNS
(`dns.alidns.com`, `223.5.5.5` / `223.6.6.6`) using DNS over HTTP/3 on UDP 443
and DNS over QUIC on UDP 853. HTTP/3 uses RFC 8484 GET `/dns-query?dns=…`;
DoQ uses the `doq` ALPN, a zero DNS transaction ID, length framing, and stream FIN.
Certificates and DNS responses are verified. No HTTP/2 or TCP fallback is used.

Each endpoint gets a cold connection followed by three fresh connections using
in-memory TLS session tickets. `0-RTT DNS` counts **sent 0-RTT packets containing
the DNS request stream**, excluding HTTP/3 setup streams. `Resumed` and acceptance
come from the TLS handshake event. A successful DNS reply after rejected early
data reflects retransmission at 1-RTT, and does not count as accepted 0-RTT.
JSON includes ticket availability, negotiated ALPN, HTTP status, DNS answers,
client ports, timings, and errors. Tickets and TLS secrets are not written out.

Each connection also handles four queries by default: an initial query, another
query after its reply, then two concurrent queries on separate streams. `Reuse`
confirms that multiple DNS queries succeeded on the same QUIC connection; `Mux`
confirms the concurrent batch succeeded. `Queries` shows successful queries over
the requested count. This checks established-connection reuse separately from
TLS session resumption on a fresh connection. JSON records each query's stream
ID, mode, HTTP status, answers, and timing.

Useful options:

```sh
# Compare changing source ports with a stable source port, retaining fresh connections.
uv run scripts/dns_0rtt_probe.py --provider alidns --all-addresses --attempts 10
uv run scripts/dns_0rtt_probe.py --provider alidns --all-addresses --attempts 10 --reuse-port

# Limit the protocol or query; list all options with --help.
uv run scripts/dns_0rtt_probe.py --provider google --protocol h3 --domain example.com

# Test one established connection with more simultaneous queries after warm-up.
uv run scripts/dns_0rtt_probe.py --all-addresses --reuse-port --queries-per-connection 6
```

A failed handshake or timeout leaves 0-RTT support **unmeasured**. Missing tickets
or tickets without early-data permission are reported separately from rejection.
Results describe the tested IPs and network path at the time of the run; anycast
backends and their resumption state can differ. The probe reports measurements
and exits successfully even when individual endpoints fail; inspect the table or
the JSON `dns_ok`, `zero_rtt_result`, and `error` fields.

Offline validation checks:

```sh
uv run --no-project --with aioquic==1.7.0 --with dnspython==2.9.0 \
  python -m unittest scripts.test_dns_0rtt_probe
```

## QUIC sniffer through TUN with curl HTTP/3

On Linux with Docker and `/dev/net/tun` available:

```sh
nix develop --command python3 scripts/test_suite/test_tun.py --http3-sniffer
```

The runner builds a static binary with `tun`, `sniffer`, and Lua routing enabled,
then creates a TUN interface inside a disposable Docker network namespace. It
mounts the Nix store read-only to use the development shell's HTTP/3-enabled curl.
The container installs aioquic 1.3.0 and runs a local HTTP/3 server with a temporary
certificate trusted explicitly by curl. No public HTTP server is contacted.

The test uses `curl --http3-only --interface sq-tun0` with a fixed destination IP
and checks the response body and negotiated HTTP version. Lua allows the request
only after asserting the TUN inbound, original destination, QUIC version, and
sniffed SNI. A second hostname selects the drop outbound and must fail. The test
also checks debug timing output and that graceful shutdown removes the interface.

Set `SHADOWQUIC_TUN_TEST_BINARY` to reuse a static binary built with the required
features. Container setup needs access to the Ubuntu and Python package indexes.
