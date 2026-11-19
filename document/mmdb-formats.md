# Inspecting country MMDB formats

Run from the repository root with your three local files:

```sh
cargo run -p shadowquic --example mmdb_inspect -- \
  /path/to/country.mmdb /path/to/geolite.mmdb /path/to/mihomo/geoip.mmdb
```

The example requires `router-db` (enabled by default). It reads each file into
memory without changing it, prints a JSON array containing its metadata, then
looks up public IPv4, private IPv4, and IPv6 addresses. Each lookup includes the
matched network, the complete raw record, and the result of decoding it with
Shadowquic's current `geoip2::Country` decoder. Three network samples are also
included so a sparse database can show records even if the requested IPs miss.

Choose addresses and the sample count explicitly:

```sh
cargo run -p shadowquic --example mmdb_inspect -- \
  country.mmdb geolite.mmdb geoip.mmdb \
  --ip 1.1.1.1,223.5.5.5 --ip 2001:4860:4860::8888 --samples 5
```

`found: false` means no record exists for the address. An IPv4-only database
reports IPv6 as unsupported. A `shadowquic_country_decode.error` reports a
record that the current country router cannot decode; `codes: []` means
there are no country or category labels to match. These compatibility results are diagnostic
and do not fail inspection. File, lookup, or raw decode errors produce a
nonzero exit status; other files are still inspected. Sampling and lookups do
not validate every record in a database.

## Container versus record schema

All of these can use the same [MaxMind DB binary container](https://maxmind.github.io/MaxMind-DB/):
an IP prefix search tree, a typed data section, and metadata at the end of the
file. Maps, strings, and arrays are all legal record types. The JSON printed by
the example is a decoded representation, not the on-disk encoding.

Filenames such as `country.mmdb`, `geolite.mmdb`, and `geoip.mmdb` do not identify
the record schema. Check `metadata.database_type` and `raw`:

| Database family | Typical decoded record | Country lookup |
| --- | --- | --- |
| MaxMind GeoLite2/GeoIP2 Country | `{"country":{"iso_code":"CN", ...}, "continent":{...}, "registered_country":{...}, ...}` | Read `country.iso_code`; other fields may be absent |
| Clash/Loyalsoldier Country MMDB | `{"country":{"iso_code":"CN"}}` | Same nested field; labels can also be categories such as `PRIVATE` or `CLOUDFLARE` |
| `sing-geoip` | `"cn"` | Read the root string |
| `Meta-geoip0` | `"cn"` or `["cn","some-other-list"]` | Read the root string or each array element; one IP can match multiple lists |

The values above illustrate shapes; country assignments and category overlap
depend on the publisher and database release. A file named `geolite.mmdb` could
also be GeoLite2 City or ASN; ASN records contain fields such as
`autonomous_system_number` and `autonomous_system_organization`, rather than
country data. The raw output exposes those differences.

[Loyalsoldier's documentation](https://github.com/Loyalsoldier/geoip#下载链接)
describes its reduced Country schema and additional categories.
[Mihomo's loader](https://github.com/MetaCubeX/mihomo/blob/Meta/component/mmdb/mmdb.go)
selects the decoder using `database_type`, and its
[reader](https://github.com/MetaCubeX/mihomo/blob/Meta/component/mmdb/reader.go)
implements the nested-object, string, and string-array cases. Its
[default configuration](https://github.com/MetaCubeX/mihomo/blob/Meta/config/config.go)
currently downloads `geoip.metadb` from MetaCubeX; this is still an MMDB
container despite the extension. A custom Mihomo download URL may use another
supported schema.

Shadowquic uses the same decoder in its direct MMDB backend, country-to-redb
importer, inspector, and IP lookup benchmark. It accepts all three record
shapes, preserving every array label as a separate case-insensitive membership.
Empty labels are ignored, and unsupported record types or non-string array
elements report decode errors. The inspector's `shadowquic_country_decode.codes`
field shows all labels accepted by this decoder.

Loyalsoldier's enhanced Country export stores one label per IP, so an additional
category can replace the geographic country. A decoder cannot recover that
overwritten label. Use a multi-label source such as MetaCubeX's `geoip.metadb`
when you need both geographic and category membership from one database.

## Trying published databases

To inspect representative published files without existing local copies:

```sh
mkdir -p /tmp/shadowquic-mmdb-inspect
curl -fL https://raw.githubusercontent.com/Loyalsoldier/geoip/release/Country.mmdb \
  -o /tmp/shadowquic-mmdb-inspect/country.mmdb
curl -fL https://raw.githubusercontent.com/Loyalsoldier/geoip/release/GeoLite2-Country.mmdb \
  -o /tmp/shadowquic-mmdb-inspect/geolite.mmdb
curl -fL https://raw.githubusercontent.com/MetaCubeX/meta-rules-dat/release/geoip.metadb \
  -o /tmp/shadowquic-mmdb-inspect/geoip.mmdb
cargo run -p shadowquic --example mmdb_inspect -- \
  /tmp/shadowquic-mmdb-inspect/country.mmdb \
  /tmp/shadowquic-mmdb-inspect/geolite.mmdb \
  /tmp/shadowquic-mmdb-inspect/geoip.mmdb
```

In a sample run, `8.8.8.8` decoded as follows (GeoLite2's extra fields omitted):

| Download | `database_type` | Raw record | Shadowquic country decoder |
| --- | --- | --- | --- |
| Loyalsoldier Country | `GeoLite2-Country` | `{"country":{"iso_code":"GOOGLE"}}` | `["GOOGLE"]` |
| GeoLite2 Country | `GeoLite2-Country` | `{"country":{"iso_code":"US"}, ...}` | `["US"]` |
| MetaCubeX geoip.metadb | `Meta-geoip0` | `["us","google"]` | `["us","google"]` |

The first two files even share a database type while assigning different labels.
Check raw records as well as metadata. Published files change over time.

## Offline smoke test

The repository includes a small MaxMind test database:

```sh
cargo run -p shadowquic --example mmdb_inspect -- \
  shadowquic/tests/fixtures/router-database/GeoIP2-Country-Test.mmdb \
  --ip 81.2.69.160 --ip 2001:218:: --ip 192.168.1.1
```

This fixture is test data, not a current geolocation database.
