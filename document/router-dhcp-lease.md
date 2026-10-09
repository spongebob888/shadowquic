# Route using DHCP leases

Build with `cargo build --release --features router-dhcp-lease`. The feature is
enabled by default, enables Lua routing, and does not require the router database feature.

Add lease sources to `router.dhcp-lease`:

```yaml
router:
  dhcp-lease:
    - type: dnsmasq
      tag: lan
      path: /tmp/dhcp.leases
  src: |
    return function(ctx)
      if ctx.src_ip_v4 then
        local host = find_dhcp_host_v4("lan", ctx.src_ip_v4)
        if host == "blocked-device" then return "block" end
      end
      return "direct"
    end
```

Configure `block` and `direct` outbounds, as in
[the complete example](../shadowquic/config_examples/router-dhcp-lease.yaml).
Use the source IP visible to the inbound; a forwarded connection may expose
an intermediary's IP rather than the original device's address.

Sources use `type: dnsmasq` (default path `/tmp/dhcp.leases`) or `type: odhcp`
(default path `/tmp/odhcpd.leases`). Both can contain IPv4 and IPv6 leases.
Relative paths resolve from the working directory. Tags must be nonempty and
unique within the lease sources; they are independent of other routing tags.

All helpers take a lease tag and a plain IP string:

| Helper | Result |
| --- | --- |
| `find_dhcp_mac_v4(tag, ip)` | Ethernet MAC |
| `find_dhcp_host_v4(tag, ip)` | Hostname |
| `find_dhcp_duid_v6(tag, ip)` | Client DUID |
| `find_dhcp_iaid_v6(tag, ip)` | Numeric IAID, including the full unsigned 32-bit range |
| `find_dhcp_mac_v6(tag, ip)` | Ethernet MAC embedded in DUID-LL or DUID-LLT |
| `find_dhcp_host_v6(tag, ip)` | Hostname |

MACs and DUIDs use lowercase colon-separated hex. Hostname case is preserved.
Missing attributes and absent/expired leases return `nil`; invalid arguments,
wrong IP families, and unknown tags raise Lua errors. No ports, CIDR suffixes,
or IPv6 zone identifiers are accepted. Helpers work during script loading
and routing with no file reads or network I/O on the lookup path.

IPv6 MAC extraction is possible only for Ethernet DUID-LL/LLT. The embedded MAC
may belong to another interface on the device; it is not a verified current
traffic MAC. DUID-EN/UUID and other unsupported structures give `nil` for MAC
lookup while their DUID remains available. Delegated prefixes are ignored;
only exact host addresses match. dnsmasq relative-duration files produced by
`HAVE_BROKEN_RTC` builds are unsupported.

Expiry is checked on every lookup. dnsmasq expiry `0` and odhcpd expiry `-1`
mean infinite; odhcpd `0` is expired. For duplicate IPs in one file, the last
still-active record wins, including across interfaces. Attributes are never
merged between records.

Files are watched and periodically reconciled every five seconds. Missing files
at startup yield empty results and are retried. Read/parse failures and deletion
retain the previous snapshot, with expiry still enforced. Infinite retained
leases remain until a successful replacement removes them. A successful empty
file clears all entries. Prefer atomic replacement: a stable, valid partial
in-place write cannot reliably be distinguished from a completed write.

Lease updates do not reset Lua state, and script reloads retain the lease store.
Each lookup sees a complete snapshot; multiple calls may straddle an update.
