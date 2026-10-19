#!/usr/bin/env bash
set -euo pipefail

# Usage: scripts/build-profiling.sh [additional cargo build arguments]
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export CARGO_PROFILE_RELEASE_DEBUG=2
export CARGO_PROFILE_RELEASE_STRIP=false

# Cargo gives encoded flags precedence over RUSTFLAGS. Preserve either form
# while enabling the Tokio instrumentation required by tokio-console.
if [[ -n "${CARGO_ENCODED_RUSTFLAGS+x}" ]]; then
    export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+${CARGO_ENCODED_RUSTFLAGS}$'\x1f'}--cfg=tokio_unstable"
else
    export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }--cfg=tokio_unstable"
fi

exec cargo build --release --locked -p shadowquic --bin shadowquic \
    --features tokio-console,mem-profile "$@"
