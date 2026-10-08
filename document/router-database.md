# Router databases

Use the [IP lookup benchmark](ip-lookup-benchmark.md) to compare direct MMDB
country lookups with ShadowQUIC's redb lookup speed and memory usage.

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
| `path` | Converted redb file, relative to the working directory |

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
separate path such as `data/country.redb`.

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
base list as well as the corresponding `list@attribute` list.

On startup, an existing redb is opened without downloading. When the file is
missing, an internal inbound with the database tag sends download connections
through the ordinary router and outbounds. HTTP redirects also pass through the
router. HTTPS validates certificates using the bundled public root store.
Downloads have a five-minute timeout, at most ten requests including redirects,
and a 512 MiB source limit. The source is converted on a blocking worker and the
completed redb is published atomically. Failed downloads/imports are logged;
lookups report the failure and a restart retries a missing database.

Lookups before import finishes raise an error, including calls made while loading
the script. Put lookups inside the returned routing function and handle startup
availability with `pcall` if fallback routing is desired. Route database download
traffic and any DNS upstream traffic needed by its outbound before these calls.
Lua script reloads retain the same database handles.

Each redb stores a format version, the creating Shadowquic version, source URL,
database type, and SHA-256 of the downloaded source bytes. Schema versions are
independent: Country uses schema `2`, while Geosite continues to use schema `1`.
Country schema `1` is unsupported; remove old converted Country files and restart
to download and rebuild them. Existing Geosite schema `1` files remain supported.

Country databases store inclusive IP ranges in two tables per lowercase ISO code:
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
and gaps between ranges return false. Geosite retains its `rules` table layout.

Incompatible, corrupt, or mismatched existing files fail startup. There is no
automatic refresh: stop
Shadowquic and remove the converted file to download again. Paths must be distinct
and writable. Imports temporarily parse the source; lookups retain only redb's
bounded 8 MiB page cache per database, not an in-memory copy of the source lists.
