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

Useful options:

```sh
# Compare changing source ports with a stable source port, retaining fresh connections.
uv run scripts/dns_0rtt_probe.py --provider alidns --all-addresses --attempts 10
uv run scripts/dns_0rtt_probe.py --provider alidns --all-addresses --attempts 10 --reuse-port

# Limit the protocol or query; list all options with --help.
uv run scripts/dns_0rtt_probe.py --provider google --protocol h3 --domain example.com
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
