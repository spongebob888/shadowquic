# IP lookup benchmark

Run from the repository root with a local country MMDB:

```sh
cargo run --release -p shadowquic --example ip_lookup_bench -- \
  --mmdb /path/to/GeoLite2-Country.mmdb --country CN \
  --queries 100000 --rounds 5 > ip-lookup.csv
```

To include the production memory-mapped MMDB backend:

```sh
cargo run --release -p shadowquic --example ip_lookup_bench -- \
  --mmdb /path/to/GeoLite2-Country.mmdb --country CN \
  --backends mmdb,mmap,redb --queries 1000000 --rounds 5 > ip-lookup.csv
```

`--backends` selects backends and their execution order. The default is
`mmdb,redb`; use `mmdb,mmap` to compare the two MMDB readers without converting
to redb. No extra Cargo feature is required beyond `router-db` (enabled by default).

`mmdb` uses `maxminddb::Reader::open_readfile`, `mmap` uses the production
`MmdbDatabase::find_ip` implementation backed by `Reader::open_mmap`,
and `redb` uses the production `RedbDatabase::find_ip` implementation with its
8 MiB page cache. All answer the same question: does this IP's
`country.iso_code` match the requested country? Both MMDB readers decode a
`geoip2::Country` record per lookup, without country-only decode optimization.
redb performs its normal
read transaction, table lookup, and range search per query. Lua dispatch, IP
string parsing, downloads, and conversion are outside lookup timing.

By default, the seeded workload mixes IPv4 and IPv6 equally. Half the queries
sample addresses uniformly from country-bearing MMDB networks (choosing
networks uniformly, not weighted by address count); half sample the entire
address space uniformly. A family without country-bearing networks uses only
uniform addresses. Use `--family v4` or `--family v6` to measure one family.
Uniform IPv6 queries often miss. The reported match count helps identify
unrepresentative workloads; this synthetic mix is not a traffic model.

For realistic traffic, provide one IP per line with `--ips traffic-ips.txt`.
Blank lines and `#` comments are allowed. This preserves order and duplicates,
overrides `--queries`, and ignores `--family`. `--country` is one membership
list for the entire run; repeat runs for other countries if needed.

The tool copies the supplied MMDB into a private temporary snapshot so its
mapped bytes cannot be affected by changes to the original file. Copying is
outside timing. It creates a fresh temporary redb only when that backend is
selected, then removes its temporary files on exit. Existing databases are untouched. Use
`--temp-dir /path/on/target/disk` to choose the storage filesystem (the system
temporary directory may be tmpfs). Conversion runs in a separate process and
its duration is printed to stderr. Each selected backend runs in its own fresh
process with the same query corpus. Each worker verifies every result against
the MMDB reference before timing; a mismatch or lookup error fails the run.
That verification pass also warms the backend. Each round replays the corpus
once, single-threaded, with `black_box` around timed inputs and results.
Legacy redb skips some IPv6 aliases that direct MMDB resolves. If a workload
hits that known difference, redb verification fails; `--backends mmdb,mmap`
allows the MMDB comparison to complete without disabling correctness checks.

CSV output includes:

| Column | Meaning |
| --- | --- |
| `backend`, `round`, `lookups`, `hits` | Backend, round number, query count, matching queries |
| `ns_per_lookup`, `lookups_per_sec` | Average latency and throughput for the complete round |
| `open_ms`, `file_bytes` | Backend open duration and database file size |
| `baseline_rss_kib` | Process resident memory after loading the query corpus, before opening the backend |
| `opened_rss_kib` | Resident memory immediately after opening the backend |
| `rss_kib` | Resident memory after the measured round |
| `peak_rss_kib` | Process lifetime resident memory high-water mark |

Memory comes from Linux `/proc/self/status`; unavailable fields are `NA`.
Values are process totals including the executable, allocator, and workload,
not exact database heap allocations. Subtract baseline RSS for an approximate
incremental cost. Peak RSS includes worker startup and corpus parsing, but
excludes source conversion and reference generation, which use other processes.
Round memory snapshots are taken outside the timed region.
For mmap, RSS includes resident file-backed pages; it is not private heap usage.
Warm lookups can fault most of the mapped database into memory, so mmap is not
expected to eliminate the database's RSS cost even though it avoids a heap copy.

These are warm-cache measurements. Filesystem cache is shared across processes
and is not flushed; backend order follows `--backends`. `open_ms` is not a cold-disk
measurement. Repeat runs on an idle machine and compare distributions across
rounds rather than treating one number as definitive.

A small offline smoke test (not representative performance data):

```sh
cargo run --release -p shadowquic --example ip_lookup_bench -- \
  --mmdb shadowquic/tests/fixtures/router-database/GeoIP2-Country-Test.mmdb \
  --country GB --queries 10000 --rounds 2
```
