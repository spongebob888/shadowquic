# Router Database Design of Shadowquic

Router can access certain database for classify certain ip and domain.

## Architecture
There is database field in router it is a list of RouterDatabaseCfg. Create a RouterDB
trait that defines the common behavior of RouterDB. It includes, find_ip(list, ip)/find_domain(list, domain) two methods. Most db only suuports one of them, in this case, the other method should always panic.

The search api should all be based on redb engine. Do not use in memory full database which cost too much memory.

Each RouterDatabaseCfg has a field of tag(should be unique among all dns/inbound/outbound tag).
It has a download link, when database doesn't exist, it will register an inbound so that router can route this download traffic after downloading finished it should convert to certain db format shadowquic used.



## Lua API
expose find_ip_v4(tag, list, ip)/find_ip_v6(tag,list, ip)/find_domain(tag, list, domain)
to lua runtime
## Rust Lib
We use redb as underlying database for each router db. 

Each redb should has a version string noting which shadowquic version created it and a sha256 of its downloading source file(Not itself ).

Schema versions are independent: Country uses schema `2`, and Geosite uses
schema `3`. Country schema `1` is unsupported. A Geosite database with a numeric
schema below `3` queues a fresh download at startup. On successful import, every
regular and multimap table, including metadata, is deleted and recreated in one
transaction. Failed downloads/imports preserve the old tables for a retry on
restart. Queries remain unavailable until rebuilding finishes. Future schema
versions, missing or nonnumeric schema markers, and mismatched database types
are not rebuilt.


## Supported RouterDB

### Country.mmdb
This is type of mmdb and convert it to redb. The list is the country ISO code. Only `country.iso_code` is stored; English country names are not stored, and records without an ISO code are skipped.

The entry of mmdb can be regarded as a list of ip ranges. In `SCHEMA = 2`, each country creates two tables named like `country_v4_us` and `country_v6_us`, including an empty table if a country has no ranges for that address family. The old schema is not supported.

Each table is a list of inclusive ip ranges with key = ip_start, value = ip_end. IPv4 tables use `u32` for both key and value; IPv6 tables use `u128`.

Merge adjacent ranges within each country and address family during import. Preserve gaps between ranges and never merge ranges belonging to different countries or address families.(Disk usage changed change from 65mb to 33mb)

Convert IP octets to integers in network (big-endian) order, independently of host endianness, using `u32::from_be_bytes` or `u128::from_be_bytes`.

To implement find_ip, select the table for the queried address family, find the greatest key that is equal to or smaller than the queried IP, and check that the IP is no larger than the range end.

Disk usage is about 129mb

### Geosite of v2fly

The `geosite` database classifies domains into named lists. It performs no DNS
resolution and does not choose an outbound; the Lua script uses membership
results to choose one. The Lua membership API is implemented by the router database backend.

Use `dlc.dat_plain.yml` from
[domain-list-community](https://github.com/v2fly/domain-list-community#download-links).
The [published YAML export](https://raw.githubusercontent.com/v2fly/domain-list-community/release/dlc.dat_plain.yml).

Full search should use indexed based fast searching, domain type searching like a.b.c should search a.b.c/b.c/c one by one.  The keyword/regex type should search one by one.

Each name corresponds to a table. The key is (SiteMatchType, Vec<u8>), and the
value is a `u32` bitmask combining zero or more `AttrType` bits.
enum SiteMatchType {
    Full,
    Domain,
    Regex,
    Keyword,
    CompiledRegex,
}
enum AttrType {
    Ads = 1 << 0,
    NotCn = 1 << 1,
    Cn = 1 << 2,
    Telemetry = 1 << 3,
}

Schema `3` names each table `geosite_<lowercase-name>`, including empty lists.
The tuple key uses `(u8, Vec<u8>)` and each value uses `u32`. Domain and
pattern keys are UTF-8 bytes. Match
types are encoded in declaration order, starting at zero. Attribute values are
`Ads = 1`, `NotCn = 2`, `Cn = 4`, and `Telemetry = 8`, corresponding to source
attributes `ads`, `!cn`, `cn`, and `telemetry`. A zero mask means no supported
attributes. Source attributes such as `:@ads,@telemetry` are combined with
bitwise OR; repeated identical rules also OR their attributes together.
Unknown attributes log a warning once per normalized name per import and are
ignored without discarding known bits or base-list membership.
`CompiledRegex = 4` keys store serialized little-endian sparse DFA bytes; values
store the attribute mask.
Patterns that cannot compile to a sparse DFA use the existing `Regex = 2` key with the UTF-8 source pattern.
Lookups deserialize compiled DFAs from keys or compile fallback source regexes.
There is no runtime regex cache.
A rule with no supported attributes has a zero mask. Unknown attributes cannot
be queried as filters. Unfiltered lookup accepts any mask; `list@attribute`
checks that attribute's bit in the stored mask. Full and domain lookups use the
tuple index, and keyword/regex lookups scan only their match-type range.
