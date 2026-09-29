#!/usr/bin/env bash
set -euo pipefail

# cross 0.2.5 applies a custom seccomp profile to 32-bit Android build
# containers. The profile embedded in cross lacks an `archMap`, so Docker/runc
# kills native 32-bit (i386) binaries with SIGSYS ("Bad system call").
#
# That breaks LuaJIT's host-side build tools (minilua/buildvm), which are
# compiled with `-m32` so their word size matches the 32-bit target.
#
# cross only writes its profile to `<target-dir>/<triple>/seccomp.json` when
# that file does not already exist, so pre-creating a corrected profile (same
# blocklist plus an archMap) fixes the build. Re-run this script after
# `cargo clean` / `cross clean`.
#
# Run from the workspace root.

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# The 32-bit Android targets built by CI. Add any other `*android*` target that
# cross treats as 32-bit here.
TARGETS=(
    i686-linux-android
    armv7-linux-androideabi
)

for target in "${TARGETS[@]}"; do
    dest="${TARGET_DIR}/${target}/seccomp.json"
    mkdir -p "$(dirname "$dest")"
    cp "${HERE}/cross-seccomp.json" "$dest"
    echo "wrote ${dest}"
done
