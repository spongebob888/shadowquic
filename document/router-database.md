# Router databases

Use the [IP lookup benchmark](ip-lookup-benchmark.md) to compare country lookup
backends, and the [geosite lookup benchmark](geosite-lookup-benchmark.md) to
measure domain search speed and memory usage.

Requires the `router-db` Cargo feature (enabled by default). This feature enables
Lua routing and the database import, download, and lookup dependencies. For Lua
routing without database support, build with `--no-default-features` and select
`plugin` alongside the other features you need.

`router.database` makes Country MMDB and v2fly Geosite lists available to Lua.
This requires the `plugin` feature. Each entry has four required fields:

| Field | Meaning |
| --- | --- |
| `tag` | Unique name across databases, inbounds, outbounds, and DNS services |
| `type` | `country` for Country MMDB, or `geosite` for the plain YAML export |
| `url` | HTTP(S) download URL |
| `path` | Persistent database file, relative to the working directory; Country `.mmdb` paths use direct MMDB lookups, other paths use redb |

```yaml
router:
  database:
    - tag: geosite-db
      type: geosite
      url: https://raw.githubusercontent.com/v2fly/domain-list-community/release/dlc.dat_plain.yml
      path: data/geosite.redb
  src: |
    return function(ctx)
      -- Downloads must route without depending on the database being downloaded.
      if ctx.inbound_tag == "geosite-db" then
        return "direct"
      end
      if ctx.dst_domain then
        -- pcall provides an explicit fallback during the first download.
        local ready, matches = pcall(find_domain, "geosite-db", "google", ctx.dst_domain)
        if ready and matches then return "proxy" end
      end
      return "direct"
    end
```

Configure the `direct` and `proxy` outbounds referenced by your script. A Country
entry uses `type: country`, a URL serving an uncompressed Country MMDB, and a
separate path. For direct MMDB lookups without conversion:

```yaml
router:
  database:
    - type: country
      tag: country-db
      path: data/country.mmdb
      # url defaults to https://git.io/GeoLite2-Country.mmdb
```

The `.mmdb` suffix is case insensitive and selects the direct backend only for
Country databases. It memory-maps the file read-only and performs MMDB lookups
against `country.iso_code`, including IPv6 aliases present in the source.
Use `data/country.redb` to retain the existing converted backend. Other suffixes
also retain redb behavior for compatibility. Geosite always uses redb.
Both country backends use the same Lua functions below.

| Lua function | Membership query |
| --- | --- |
| `find_domain(tag, list, domain)` | Geosite list, including attribute lists such as `google@ads` |
| `find_ip_v4(tag, list, ip)` | Country of an IPv4 address string |
| `find_ip_v6(tag, list, ip)` | Country of an IPv6 address string |

The helpers return booleans and perform no DNS resolution. Country lists accept
ISO codes (such as `US`) only; English country names are not stored. List
names and domain names are case insensitive; a trailing domain dot is ignored.
Unknown lists return false. Unknown database tags, unavailable databases, invalid
IP strings, or a helper used with the wrong database type raise Lua errors.

Geosite `full` rules match exactly. `domain` rules match the domain and its
subdomains on label boundaries. Both use redb indexes. `keyword` and `regexp`
rules are scanned within the requested list. Regular expressions use Rust's
`regex` syntax and are validated during import. Attributes remain part of the
base list. Regexes are compiled to DFAs during import and stored in redb values.
The supported attribute filters are `list@ads`, `list@!cn`, and
`list@cn`; other attribute filters return false. Rules carrying only unsupported
attributes remain in the base list.

On startup, an existing MMDB or redb is opened without downloading. When the file is
missing, an internal inbound with the database tag sends download connections
through the ordinary router and outbounds. HTTP redirects also pass through the
router. HTTPS validates certificates using the bundled public root store.
Downloads have a five-minute timeout, at most ten requests including redirects,
and a 512 MiB source limit. On a blocking worker, country `.mmdb` downloads are
validated and saved unchanged; redb sources are converted. The completed file is
published atomically without replacing an existing file. Failed downloads/imports are logged;
lookups report the failure and a restart retries a missing database.

Lookups before import finishes raise an error, including calls made while loading
the script. Put lookups inside the returned routing function and handle startup
availability with `pcall` if fallback routing is desired. Route database download
traffic and any DNS upstream traffic needed by its outbound before these calls.
Lua script reloads retain the same database handles.

Each redb stores a format version, the creating Shadowquic version, source URL,
database type, and SHA-256 of the downloaded source bytes. Schema versions are
independent: Country and Geosite both use schema `2`. Country schema `1` and
Geosite schema `1` are unsupported; remove old converted files and restart to
download and rebuild them.

Converted Country redb databases store inclusive IP ranges in two tables per lowercase ISO code:
`country_v4_us` and `country_v6_us`, for example. Records without a country ISO
code are skipped. Existing files containing English-name tables must be rebuilt
to reclaim that space.
Adjacent ranges belonging to the same country and address family are merged
during import. Gaps remain unmatched. Rebuild existing files to apply this
storage optimization; the Country schema remains `2`.
IPv4 tables map `ip_start: u32` to `ip_end: u32`; IPv6 tables map
`ip_start: u128` to `ip_end: u128`. Both tables are created even if one is empty.
Addresses use their numeric network-order values, computed with
`from_be_bytes(ip.octets())` independently of host endianness. A lookup selects the table
for the query's address family and the greatest start key no larger than the
query, then checks the inclusive end. Missing tables
and gaps between ranges return false.

Geosite schema `2` stores each base list in a table named `geosite_<lowercase-name>`,
including empty lists. Keys are `(SiteMatchType, Vec<u8>)`; domains and
fallback regex patterns use UTF-8 bytes. Values are `u8` attribute enums.
Match types are
`Full = 0`, `Domain = 1`, `Regex = 2`, `Keyword = 3`, `CompiledRegex = 4`;
attributes are
`Nil = 0`, `Ads = 1`, `NotCn = 2`, `Cn = 3`. Each rule has one row and at most
one attribute. Rules with no supported attributes use `Nil`. `CompiledRegex = 4`
keys store serialized little-endian sparse DFA bytes; their values store the
attribute enum directly. Existing schema `2` files with byte-slice values or
dense `CompiledRegex` records must be rebuilt. Patterns that cannot compile to
a sparse DFA use the existing `Regex = 2` key with the UTF-8 source pattern.
Compiled DFAs are decoded directly from keys during lookup, and fallback
regexes are compiled during lookup; there is no runtime regex cache.
Unfiltered queries match any
attribute. Full and domain lookups use the tuple index, while keyword and regex
lookups scan only the corresponding match type in the requested list table.

Direct MMDB files contain no Shadowquic schema or source-URL metadata; changing
`url` does not invalidate an existing MMDB. Invalid MMDB headers/metadata fail
startup; record decoding errors are reported by the lookup. Direct MMDB uses the
source's IPv6 alias behavior, while legacy redb conversion continues to skip
aliased ranges.

Incompatible, corrupt, or mismatched existing redb files fail startup. There is no
automatic refresh: stop
Shadowquic and remove the database file to download again. Paths must be distinct
and writable. To switch an existing redb configuration to direct MMDB, choose a
new `.mmdb` path; renaming a redb file does not convert it into MMDB.
Direct MMDB uses a read-only memory mapping instead of copying the entire file
into a heap buffer. Accessed pages still count toward resident memory (RSS).
Do not modify or truncate an MMDB while Shadowquic is running: stop Shadowquic
before updating the file. The mapping stays alive across Lua script reloads.
redb imports temporarily parse the source; redb lookups retain a bounded 8 MiB
page cache per database.
