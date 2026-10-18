---
name: deploy-shadowquic-musl
description: Build Shadowquic as a static musl release, copy it over SSH, and replace an authorized running instance with backup, health checks, and rollback. Use for remote Shadowquic binary deployments.
---

# Deploy Shadowquic musl

Run from the repository root. Build the current working tree, including requested
uncommitted changes. A deployment request authorizes the requested replacement;
this skill alone does not authorize restarting an unrelated instance.

## Target and discovery

The established deployment target is `root@192.168.7.199`, with executable
`/root/shadowquic/shadowquic`. Use another target when the user specifies one.
Use `ssh -F /dev/null` and `scp -F /dev/null` for this host to avoid local SSH
configuration changing the route. Do not disable host key checking.

Before replacing anything, inspect the remote architecture, exact process command
line, working directory, stdout/stderr destinations, and service supervisor.
Record the current PID and executable checksum; resolve multiple instances before
selecting one. Inspect relevant environment settings without printing secrets.
Hash `config.yaml` and `router.lua` when present so they can be checked afterward.

Previously this host was x86_64, running `./shadowquic` from `/root/shadowquic`,
with default `config.yaml` and output in `nohup.out`, without a systemd service.
Rediscover these facts on each deployment; do not reuse a historical PID.

Capture a baseline of API availability and proxy connectivity. This host has used
API port `9090` and SOCKS port `1089`; confirm its current listeners. For example:

```sh
ssh -F /dev/null root@192.168.7.199 \
  'curl --fail --silent --show-error --max-time 20 --socks5-hostname 127.0.0.1:1089 -o /dev/null -w "HTTP %{http_code}\n" https://example.com/'
```

## Build and stage

For the x86_64 host, use the repository's Nix musl package:

```sh
nix build .#musl --no-link --print-out-paths
```

The output executable is `<store-path>/bin/shadowquic`. `flake.nix` selects
`pkgsStatic`, release optimization, `plugin-system` with static LuaJIT, and the
normal protocol, REST API, TUN, statistics, and jemalloc features. No profiling
flags are needed. A dirty-tree warning is expected for uncommitted source edits.
The Nix package sets `doCheck = false`; report relevant Rust tests separately.
For another architecture, select the matching musl target/toolchain before building;
the local `.#musl` package does not imply arbitrary remote architecture support.

Verify `file`/`readelf` identify the expected architecture and static linkage
(no ELF interpreter or dynamic `NEEDED` libraries). Run `--help` locally when
host-compatible. Compute SHA-256, copy to a unique adjacent staging path such as
`/root/shadowquic/shadowquic.new-<UTC timestamp>`, set executable permissions,
and compare the remote checksum. Stage and verify before stopping the old process.

## Replace and recover

1. Recheck the selected process still uses the expected executable and command.
   Create a timestamped executable backup in the same directory and record hashes
   of the binary and configuration. Preserve configuration, router scripts,
   databases, network rules, and unrelated processes.
2. If supervised, use the existing service's stop/start controls. Otherwise send
   `SIGTERM` to the selected PID; Shadowquic handles it as graceful shutdown. Wait
   a bounded interval for exit before proceeding. If shutdown fails, leave the
   installed binary intact and diagnose instead of launching a duplicate instance.
3. Rename the verified staged executable onto the destination on the same filesystem.
   Do not copy over an executing binary. Restart with the original working
   directory, arguments, environment, and supervision method. For the discovered
   unmanaged setup, detach stdin and both output streams with `nohup`, append to
   the existing log, and record `$!` in `shadowquic.pid`.
4. Poll readiness with a timeout. Verify the new PID remains alive, its `/proc/PID/exe`
   checksum matches the build, expected listeners return, and startup logs contain
   no fatal error. Check config/router hashes still match.
5. Check `GET /api/v1/tokio_metric` using the configured bearer token if required.
   Expect HTTP 200 and unsigned numeric `num_workers`, `num_alive_tasks`, and
   `global_queue_depth`. Also check an existing API such as `/api/v1/traffic` and
   repeat the baseline SOCKS request. Keep credentials out of output.

If the new process fails startup or health checks, stop that specific new process
before restoring the backup and restarting the original launch setup. Verify the
restored instance against the baseline and report the failure. Attempt rollback
once; if recovery fails, preserve artifacts/logs and report the exact state rather
than repeatedly restarting or changing configuration.

Report the build artifact, deployed path, new PID, backup path, health results,
and any unresolved issue. Keep the backup available for manual recovery.
