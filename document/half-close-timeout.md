# Outbound half-close timeout

`direct`, `socks`, `shadowquic`, and `sunnyquic` outbounds accept
`half-close-timeout`, a nonnegative integer in milliseconds. The default is `60000` (60 seconds). Set it to `0` to disable the timeout.

```yaml
outbounds:
  - tag: direct
    type: direct
    half-close-timeout: 30000
```

The timer starts when either source of a TCP relay reaches EOF (a half-close).
Every successful nonempty read or write resets it. The other direction can
continue sending a response for as long as it makes progress. If no bytes are
read or written for the configured interval, the relay exits with a timeout
and drops both streams, even if a write or shutdown is blocked.

This is an inactivity timeout, not a maximum response duration. It does not
limit idle connections before EOF, affect UDP forwarding, or close the shared
QUIC connection used by other streams. Each outbound has its own setting.
