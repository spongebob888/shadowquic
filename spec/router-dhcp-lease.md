# Router DHCP lease files

Expose local dnsmasq and odhcpd lease data to Lua routing scripts. Each file can
contain both DHCPv4 and DHCPv6 leases. Lookups match exact IP addresses and do
not perform DNS resolution, neighbor discovery, or network requests.

## Configuration

Gate this functionality behind Cargo feature `router-dhcp-lease`, which enables
`plugin`, does not require `router-db`, and is enabled by default. Without
this feature, nonempty `router.dhcp-lease` configuration is an error and DHCP
Lua helpers are absent.

`router.dhcp-lease` is a list of `DhcpLeaseCfg`, defaulting to an empty list.
Use an internally tagged Serde enum with discriminator `type`, kebab-case
names, and unknown-field rejection on the configuration structs:

```rust
enum DhcpLeaseCfg {
    Dnsmasq(DnsmasqLeaseCfg),
    Odhcp(OdhcpLeaseCfg),
}

struct DnsmasqLeaseCfg {
    pub tag: String,
    /// Defaults to /tmp/dhcp.leases.
    pub path: PathBuf,
}

struct OdhcpLeaseCfg {
    pub tag: String,
    /// Defaults to /tmp/odhcpd.leases.
    pub path: PathBuf,
}
```

```yaml
router:
  dhcp-lease:
    - type: dnsmasq
      tag: lan-dnsmasq
      path: /tmp/dhcp.leases
    - type: odhcp
      tag: lan-odhcpd
      path: /tmp/odhcpd.leases
  src: |
    return function(ctx)
      if ctx.src_ip_v4 then
        local host = find_dhcp_host_v4("lan-dnsmasq", ctx.src_ip_v4)
        if host == "workstation" then
          return "proxy"
        end
      end
      return "direct"
    end
```

The example assumes `proxy` and `direct` outbounds are configured. `odhcp` is
the configuration type for the odhcpd parser. Paths are configurable; defaults
are conveniences, not guarantees about the producer's configuration. Relative
paths resolve from the process working directory. Empty paths and empty or
duplicate lease tags are errors. Lease tags have a separate namespace from DNS,
inbound, outbound, and router database tags. They create no routing destination.

## Lua API

All helpers accept `(tag, ip)` as strings. Require a plain IP of the specified
family without a port, CIDR suffix, or IPv6 zone identifier. Unknown tags,
malformed addresses, and wrong address families raise Lua errors.

| Helper | Result for an active matching lease |
| --- | --- |
| `find_dhcp_mac_v4(tag, ip)` | Ethernet MAC as lowercase colon-separated hex, or `nil` |
| `find_dhcp_host_v4(tag, ip)` | Hostname string, or `nil` |
| `find_dhcp_duid_v6(tag, ip)` | Client DUID as lowercase colon-separated hex, or `nil` |
| `find_dhcp_iaid_v6(tag, ip)` | IAID as a Lua number in `0..=4294967295` |
| `find_dhcp_mac_v6(tag, ip)` | Ethernet MAC embedded in a supported DUID, or `nil` |
| `find_dhcp_host_v6(tag, ip)` | Hostname string, or `nil` |

Every helper returns `nil` when no active lease matches, including when a known
tag has no successfully loaded snapshot. Missing attributes do not suppress
other attributes. Treat dnsmasq `*` and odhcpd `-` hostname placeholders as
missing; preserve real hostname case. Decode odhcpd `\xNN` hostname escapes;
hostnames marked invalid by its `broken\x20` prefix return `nil`.

IPv6 MAC extraction supports only DUID-LLT (type 1) and DUID-LL (type 3), with
Ethernet hardware type 1 and exactly six address bytes. Validate the complete
structure. Unsupported types (including DUID-EN and DUID-UUID) or structurally
invalid DUIDs return `nil` for MAC lookup. Never extract the final six bytes
unconditionally. This MAC identifies the interface used to construct the DUID;
it is not guaranteed to identify the client's current traffic interface.

Normalize IAIDs to numbers despite different producer text bases. A dnsmasq
`T` prefix is not part of the numeric IAID. DUID lookup returns the client's
identifier, never the standalone dnsmasq server DUID. Syntactically valid DUID
bytes remain available even if their type cannot be interpreted for MAC lookup.

Helpers are available during script initialization and routing. Use shared
in-memory snapshots, with no file I/O or asynchronous suspension on the lookup
path. Each call observes a complete snapshot; separate calls can observe
different snapshots if an update occurs between them. Lua script reloads retain
the lease store and watchers. Lease updates do not reset Lua state or alter
existing connections.

## dnsmasq format

One file can contain IPv4, IPv6, and server metadata. Split on whitespace and
distinguish lease address families by the third field, not the second:

```text
IPv4: expiry MAC IPv4 hostname client-id
IPv6: expiry IAID IPv6 hostname client-DUID
Server metadata: duid server-DUID
```

IPv6 IAIDs are decimal unsigned 32-bit integers, optionally prefixed with `T`
for temporary addresses. DUIDs and client IDs use colon-separated hexadecimal
bytes. The IPv4 client ID is not a substitute for a MAC; non-Ethernet or absent
hardware addresses yield `nil` for MAC lookup.

Example file (format labels above are explanatory, not file contents):

```text
1791593157 02:00:00:00:00:54 192.168.2.216 * 01:02:00:00:00:00:54
1791593163 02:00:00:00:00:98 192.168.2.240 workstation *
duid 00:03:00:01:02:00:00:00:00:01
1791593163 42 2001:db8::98 workstation 00:03:00:01:02:00:00:00:00:98
1791593163 T43 2001:db8::99 * 00:03:00:01:02:00:00:00:00:98
```

Ignore blank lines, comment lines, and known metadata records (`duid`,
`vendorclass`, `agent-info`). Missing client DUID (`*`) still permits hostname
and IAID lookup. Support normal absolute-expiry files only. dnsmasq builds
using `HAVE_BROKEN_RTC` write relative lease lengths and are outside this spec;
that variant cannot reliably be detected from the file alone.

## odhcpd format

IPv4 and IPv6 share an overall layout. Lease records start with literal `#`;
do not discard them as comments:

```text
IPv6: # interface DUID IAID hostname expiry assigned_id prefix_length address/prefix ...
IPv4: # interface MAC ipv4 hostname expiry address_hex 32 address/32
```

The literal `ipv4` in the third field after `#` identifies IPv4. Otherwise that
field is a hexadecimal unsigned 32-bit IAID. IPv6 DUIDs use hexadecimal bytes
without separators; IPv4 MACs use colon-separated hex. `assigned_id` is a
hexadecimal host or subnet identifier and `prefix_length` is decimal. Validate
these fields, but do not synthesize addresses from them.

```text
# br-lan 02:00:00:00:00:98 ipv4 workstation 1791593163 c0a802f0 32 192.168.2.240/32
# br-lan 000100012c444e2226f111bbf0d6 8aaaad6 CT103 1791556279 b3b 128 2001:db8:1::b3b/128 fd00::b3b/128
# br-lan 000100012c444e2226f111bbf0aa 5aaa5b - 1791557390 b94 128 2001:db8:1::b94/128 fd00::b94/128
```

Index every listed host address: IPv4 `/32` and IPv6 `/128`. Ignore delegated
prefix records (IPv6 `prefix_length < 128`); they do not identify individual
hosts within the prefix. Do not turn prefixes into hosts by stripping the
suffix. IPv6 records without addresses create no entries.

Ignore blank lines, ordinary comments, and non-`#` hosts-file lines in older
odhcpd output. A `#` line is a lease candidate if it has at least eight fields
after `#`, or its third field after `#` is `ipv4`. Invalid lease candidates
are parse errors; shorter non-lease comments are ignored.

## Expiry and conflicts

Finite expiry values are Unix timestamps in seconds. A lease is active only
while `expiry > current_time`. dnsmasq `0` means infinite; odhcpd `-1` means
infinite and `0` means expired. Reject other negative expiry values.

Enforce expiry on every lookup, even without file events or after failed
reloads. Use a wide timestamp representation without a 2038 limit. Keep tags
independent.

For duplicate IPs within one file, the last active record in file order wins
as a complete record. Do not merge attributes across records. Retain enough
information to select the last still-active record as candidates expire.
This also applies across interfaces: the API has no interface parameter.
Deployments requiring separate identity domains must use separate files/tags.

## Loading and automatic updates

Attempt an initial load before Lua script evaluation. Missing, unreadable, or
malformed files cause a warning and an empty initial store, not startup failure.
Configuration validation errors remain fatal.

Watch parent directories to survive atomic replacement and deletion/recreation.
Reread after watch registration to close the initial-read/watch race. Coalesce
event bursts and serialize reloads per tag so older reads cannot overwrite newer
snapshots. Retry failed reads and watch setup. Reconcile periodically at least
every five seconds to recover missed events and files or parent directories
created after startup.

Parse into a new snapshot and publish atomically after successful validation.
A malformed lease record rejects the whole candidate; log its tag and line
number. Failed reads, deletion, and rejected candidates retain the last
successful snapshot, whose finite leases still expire. Successful empty files
clear entries; successful replacements remove records absent from the new file.
Infinite leases retained after failure remain available until a successful
replacement removes them.

Debounce and retry files observed changing during a read. Producers should use
atomic replacement for consistent updates: a watcher cannot reliably distinguish
a stable empty or truncated-but-valid file from a completed write. Do not claim
transactional reads of arbitrary in-place writes.

Watchers and reconciliation tasks stop when their owning lease store is dropped.
No persistent database or downloaded data is required.

## Acceptance tests

- Configuration defaults, discriminators, unknown fields, empty/duplicate tags,
  and feature-disabled configuration rejection and helper absence.
- Mixed dnsmasq IPv4/IPv6, metadata, decimal and `T`-prefixed IAIDs, missing
  attributes, and non-Ethernet hardware addresses.
- Mixed odhcpd IPv4/IPv6, `ipv4` marker, hexadecimal IAIDs, escaped/invalid
  hostnames, multiple addresses, and ignored delegated prefixes.
- DUID-LL/LLT MAC extraction, unsupported/structurally invalid DUIDs, normalized
  client DUIDs, and maximum unsigned IAID.
- Invalid API arguments, unknown tags, missing IPs, and hostname placeholders.
- Expiry without file changes, infinite values, and duplicate precedence as
  records expire, using a controllable clock.
- Missing files/directories at startup, edits, atomic replacement, deletion and
  recreation, empty replacement, malformed reloads, and missed-event recovery.
- Atomic snapshot visibility, continued expiry after failed reloads, Lua reload
  persistence, and background task cleanup.

## Format references

- [dnsmasq lease reader and writer](https://raw.githubusercontent.com/imp/dnsmasq/master/src/lease.c)
- [odhcpd state-file writer](https://raw.githubusercontent.com/openwrt/odhcpd/master/src/statefiles.c)
- [DHCPv6 DUID formats](https://www.rfc-editor.org/rfc/rfc8415.html#section-11)

The explicit contracts above define the supported subset. Capture them in
fixtures rather than relying on upstream branch contents remaining unchanged.
