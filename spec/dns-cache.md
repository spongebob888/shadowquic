# DNS cache refactor

Replace the hand-managed DNS response cache and fake-IP mapping storage with
bounded caches using the `quick_cache` crate. Accept the crate's native eviction
policy; strict least-recently-used eviction is not required.

## DNS response cache

- Preserve the manager-owned cache shared by non-fake-IP resolvers, with a
  maximum of 4,096 responses. Use unit entry weights so capacity counts responses.
- Preserve query keys with only the transaction ID cleared, restoration of the
  requesting client's transaction ID, and TTL aging on cache hits.
- Preserve positive-answer admission rules: do not cache failed, truncated,
  empty, or zero-TTL answers. Expire a response at its shortest record TTL.
- Preserve `bypass-cache`: bypassing services neither read nor write the shared
  cache and leave existing entries untouched. Fake-IP responses remain excluded
  from the shared response cache.
- Preserve `lookup_cache` and `reverse_lookup_cache`, including case-insensitive
  domain matching, CNAME handling, PTR handling, and selection of the most
  recently inserted matching response for reverse lookup. These methods must
  never perform network I/O. Scanning entries for these helpers must not promote
  every inspected entry in the eviction policy.

### Expiry and cleanup

Expiry checks are mandatory on every read, independently of cleanup. `get`,
`lookup_cache`, and `reverse_lookup_cache` must never return expired data.
Reading an entry does not extend its lifetime.

Perform an opportunistic scan for expired entries at most once every 60 seconds,
triggered by `get`, `insert`, `lookup_cache`, or `reverse_lookup_cache`. Use a
monotonic clock and initialize the last-scan timestamp when the cache is created.
No background task is required; expired entries may remain allocated while the
cache is idle, subject to the capacity limit.

Only one caller may perform cleanup at a time. Callers that do not claim cleanup
should proceed with their normal operation. Removal must check the current
entry's expiry atomically with removal, or use equivalent synchronization, so
a scan cannot delete a fresh replacement of an expired entry. Ordinary operations
must no longer perform a full expiry scan on every call.

## Fake-IP mapping cache

Maintain a separate cache with a hard maximum of 65,536 live domain mappings,
reduced for small configured pools as described below.
One mapping contains a normalized domain and its shared IPv4/IPv6 allocation
index; A and AAAA addresses together count as one mapping. Preserve normalization
by lowercasing the domain and removing trailing dots. Retain the 60-second DNS
answer TTL. Accept optional `ipv4-range` and `ipv6-range` CIDRs on `dns-fakeip`,
defaulting to `198.18.0.0/15` and `2001:0002::/48`. Validate address families, prefix lengths, zero
host bits, and a minimum of four addresses in each range. Reserve the first and
last address in each range. Paired IPv4/IPv6 mappings share the same offset from
their respective network addresses, with the usable index count determined by
the smaller range's address count minus two. Limit resident mappings to
`min(65,536, usable_indices - 1)` so allocation always has a free index before
capacity eviction.

Repeated allocations for a resident domain return the same addresses. Successful
domain allocations and reverse restoration of fake addresses should record use
with the cache's native eviction policy. Fake-IP mappings have no time-based
expiry; the DNS answer TTL does not determine whether an address is safe to reuse.

Forward and reverse mappings must remain consistent through insertion,
eviction, and concurrent access. Evict both address-family mappings together.
Any auxiliary reverse index must also be bounded by the number of live mappings;
it must not retain evicted domain names. Ensure a newly returned DNS address has
a corresponding reverse mapping when allocation completes.

### Eviction and address reuse

Allocate sequentially through the configured usable indices (131,070 by default),
wrapping from the last index back to index 1. Skip any index with a resident
mapping; never replace a resident domain solely to reuse its index. The mapping
capacity is smaller than the pool, so an available index always exists. If an evicted domain is queried
again, assign the next available index rather than remembering its old mapping.

An evicted fake address must fail restoration as an unknown fake IP until its
index is reused; never forward an unmapped address inside a configured fake-IP
range as a real destination. Addresses outside those ranges pass through
unchanged. Reverse lookup must check range membership and the full offset before
converting it to an allocation index.
Once an index is reused after wrapping, old DNS answers for that index restore
to the new domain. Address stability is guaranteed only while a mapping remains
resident. Clients retaining evicted answers, including subsequent packets of an
existing UDP flow, can therefore fail or target the new domain after reuse.
Already restored destinations are unaffected. Preserve the destination port on
successful restoration. Mappings remain unpersisted across manager replacement
or process restart.

## Validation and documentation

Keep existing DNS cache, bypass, reverse-lookup, and fake-IP integration tests
passing, except where lifetime stability is intentionally replaced by residency
stability. Add regression coverage for:

- Expiry between cleanup scans, TTL aging, and transaction-ID restoration.
- Cleanup throttling, removal of expired entries, and preservation of concurrently
  refreshed entries.
- Response-cache capacity and replacement of an existing key at capacity.
- Fake-IP capacity measured in domains, dual-stack consistency, normalization,
  and concurrent allocation of the same domain.
- Configured CIDR defaults and independent overrides, invalid range rejection,
  DNS answers and TCP/UDP restoration from custom ranges, small-pool wrapping,
  reserved addresses, and offset arithmetic for extreme prefixes.
- Consistent eviction of forward and reverse mappings, rejection of evicted
  addresses before reuse, wrapping at the last index, skipping resident indices,
  and consistent dual-stack restoration after an evicted index is reused.

Use small injectable capacities and controllable time in tests where useful;
tests must not depend on sleeps or a strict LRU victim order.

Update `document/dns.md` to describe the eviction policy, cleanup behavior,
65,536-domain mapping limit and reductions for small pools, configurable ranges,
residency-based stability, and index reuse after wrapping. Keep the implementation
behind the existing DNS feature gates.
