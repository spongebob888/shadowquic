#!/usr/bin/env bash
set -euo pipefail

# Usage: scripts/build-profiling.sh [--mem-profile-only] [additional cargo build arguments]
features=tokio-console,mem-profile
cargo_args=()
while (( $# )); do
    case "$1" in
        --mem-profile-only) features=mem-profile ;;
        --)
            shift
            cargo_args+=("$@")
            break
            ;;
        *) cargo_args+=("$1") ;;
    esac
    shift
done

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

export CARGO_PROFILE_RELEASE_DEBUG=1
export CARGO_PROFILE_RELEASE_STRIP=false

# Cargo gives encoded flags precedence over RUSTFLAGS. Preserve either form
# while enabling the Tokio instrumentation required by tokio-console.
if [[ "$features" == tokio-console,mem-profile ]]; then
    if [[ -n "${CARGO_ENCODED_RUSTFLAGS+x}" ]]; then
        export CARGO_ENCODED_RUSTFLAGS="${CARGO_ENCODED_RUSTFLAGS:+${CARGO_ENCODED_RUSTFLAGS}$'\x1f'}--cfg=tokio_unstable"
    else
        export RUSTFLAGS="${RUSTFLAGS:+${RUSTFLAGS} }--cfg=tokio_unstable"
    fi
fi

# GNU ld.bfd can reject debug-only Rustls references after fat LTO. Use the
# toolchain's LLVM linker for x86_64 musl, retaining full debug information.
target="${CARGO_BUILD_TARGET:-}"
for (( i=0; i<${#cargo_args[@]}; i++ )); do
    case "${cargo_args[i]}" in
        --target) target="${cargo_args[i+1]:-}" ;;
        --target=*) target="${cargo_args[i]#--target=}" ;;
    esac
done
if [[ "$target" == x86_64-unknown-linux-musl ]]; then
    rustc="${RUSTC:-rustc}"
    host="$("$rustc" -vV | sed -n 's/^host: //p')"
    lld_dir="$("$rustc" --print sysroot)/lib/rustlib/$host/bin/gcc-ld"
    if [[ ! -x "$lld_dir/ld.lld" ]]; then
        printf 'error: LLVM linker not found at %s/ld.lld; install a Rust toolchain with rust-lld\n' "$lld_dir" >&2
        exit 1
    fi
    exec cargo rustc --release --locked -p shadowquic --bin shadowquic \
        --features "$features" "${cargo_args[@]}" -- \
        -C "link-arg=-B$lld_dir" -C link-arg=-fuse-ld=lld
fi

exec cargo build --release --locked -p shadowquic --bin shadowquic \
    --features "$features" "${cargo_args[@]}"
