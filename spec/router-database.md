# Router Database Design of Shadowquic

Router can access certain database for classify certain ip and domain.

## Architecture
There is database field in router it is a list of RouterDBCfg. Create a RouterDB
trait that defines the common behavior of RouterDB. It includes, find_ip(list, ip)/find_domain(list, domain) two methods. Most db only suuports one of them, in this case, the other method should always panic.

The search api should all be based on redb engine. Do not use in memory full database which cost too much memory.

Each RouterDBCfg has a field of tag(should be unique among all dns/inbound/outbound tag).
It has a download link, when database doesn't exist, it will register an inbound so that router can route this download traffic after downloading finished it should convert to certain db format shadowquic used.



## Lua API
expose find_ip_v4(tag, list, ip)/find_ip_v6(tag,list, ip)/find_domain(tag, list, domain)
to lua runtime
## Rust Lib
We use redb as underlying database for each router db. 

Each redb should has a version string noting which shadowquic version created it and a sha256 of its downloading source file(Not itself ).


## Supported RouterDB

### Country.mmdb
This is type of mmdb and convert it to redb. The list is the country name.

### Geosite of v2fly

The `geosite` database classifies domains into named lists. It performs no DNS
resolution and does not choose an outbound; the Lua script uses membership
results to choose one. This section defines a proposed API, not functionality
already implemented.

Use `dlc.dat_plain.yml` from
[domain-list-community](https://github.com/v2fly/domain-list-community#download-links).
The [published YAML export](https://raw.githubusercontent.com/v2fly/domain-list-community/release/dlc.dat_plain.yml).

Full search should use indexed based fast searching, domain type searching like a.b.c should search a.b.c/b.c/c one by one.  The keyword/regex type should search one by one.