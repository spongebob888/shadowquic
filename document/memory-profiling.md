# Memory profiling

ShadowQUIC's default Cargo feature set uses jemalloc. The allocator is installed
only by the `shadowquic` executable, so applications using the library remain in
control of their process-wide allocator.

## Allocator statistics

jemalloc can print a summary when the process exits normally. Start a release
build with statistics enabled, exercise representative traffic, then stop it
with `SIGINT` (Ctrl-C):

```bash
cargo build --release -p shadowquic
_RJEM_MALLOC_CONF=stats_print:true \
  ./target/release/shadowquic -c shadowquic/config_examples/client.yaml
```

The most useful values are:

- `allocated`: bytes currently used by the application.
- `active`: pages backing live allocations; the difference from `allocated`
  approximates allocator fragmentation.
- `resident`: physical memory retained by jemalloc, including metadata and
  unused pages. This is only part of process RSS.
- `mapped` and `retained`: virtual address space mapped or retained by the
  allocator. They are not live heap usage.

RSS also includes thread stacks, executable and shared-library pages, memory
maps, and kernel socket buffers. Do not treat RSS minus `allocated` as a leak.
For a leak investigation, compare `allocated` after identical traffic and an
idle settling period; a monotonic increase across repeated cycles is more
meaningful than a single RSS reading.

## Heap profiles

Build jemalloc with profiling support:

```bash
cargo build --release -p shadowquic --features jemalloc-profiling
```

Run a representative workload and request periodic heap dumps:

```bash
_RJEM_MALLOC_CONF=prof:true,prof_active:true,lg_prof_interval:30,prof_prefix:/tmp/shadowquic-jeprof \
  ./target/release/shadowquic -c shadowquic/config_examples/client.yaml
```

`lg_prof_interval:30` writes a profile after roughly every GiB of allocation,
not every 30 seconds. Lower it for a short, low-volume test. Analyze a generated
profile with a jemalloc-compatible `jeprof`, for example:

```bash
jeprof --text ./target/release/shadowquic /tmp/shadowquic-jeprof.*.heap
jeprof --svg ./target/release/shadowquic /tmp/shadowquic-jeprof.*.heap > heap.svg
```

Keep debug information when meaningful stack traces are needed. The workspace
release profile currently strips binaries, so use a separate profiling profile
or temporarily set `strip = false` before collecting the final profile.

To build without jemalloc, disable default features and explicitly select the
application features needed by that build.
