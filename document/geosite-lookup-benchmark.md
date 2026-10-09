# Geosite lookup benchmark

Run from the repository root with a local v2fly geosite YAML source:

```sh
cargo run --release -p shadowquic --example geosite_lookup_bench -- \
  --source /path/to/dlc.dat_plain.yml --list google \
  --queries 100000 --rounds 5 > geosite-google.csv
```

The benchmark compares production `RedbDatabase::find_domain` against a
precompiled in-memory scan of the selected list's rules. Both use the same
`full`, `domain`, `keyword`, and `regexp` matching semantics, including list
attributes. Regexes are compiled once for the in-memory scan. The geosite
importer stores serialized sparse DFA bytes under the `CompiledRegex` key
type. Patterns unsupported by DFA compilation use the `Regex` key type and
retain their source pattern. Lookups deserialize compiled DFAs or compile
fallback regexes on demand; the runtime does not cache regexes. This compares
indexed lookups against a linear scan.

The source is imported into a temporary redb before the timed runs; conversion
time is printed on stderr and excluded from lookup timings. The original source
file is not modified. Repeated list names are combined case insensitively, as
they are during redb import. The redb import still processes the whole source.

By default, the seeded workload alternates candidate matches and likely misses.
It creates hit candidates from each rule type where possible, including common
domains for regular expressions. Use `--domains domains.txt` for real traffic or
a controlled workload. That file accepts one domain per line, blank lines, and
`#` comments; order and duplicates are preserved, and `--queries` is ignored.
For example, benchmark an attribute list with `--list google@ads`.

Each backend runs in its own process, replays the same corpus serially, and
verifies every answer against the precompiled in-memory reference before
timing. A mismatch fails the run. The verification pass warms the backend.
CSV reports round, query count, matches, nanoseconds per lookup, lookups per
second, backend-open time, and baseline/open/after-run/peak RSS where Linux
`/proc/self/status` is available. RSS is total process memory, including the
workload and runtime; it is not an allocation-only measurement.

The default OS temporary directory holds the converted database and workload.
Choose `--temp-dir /path/on/target/disk` to select another filesystem. Measurements
use warm filesystem caches and are single threaded. Repeat runs on an idle
machine and compare the rounds rather than treating one round as definitive.
