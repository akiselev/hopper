#!/usr/bin/env bash
# Build every fixture under fixtures/ (debug, opt-level 0, full debuginfo) into
# target/fixtures/<name>/. Prints the path of each binary.
set -euo pipefail
# The fixtures' own profiles set full debuginfo; a caller's override (CARGO_PROFILE_DEV_DEBUG=0,
# used to keep hopper's own builds small) must not strip it.
unset CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_DEV_OPT_LEVEL
root="$(cd "$(dirname "$0")/.." && pwd)"
for dir in "$root"/fixtures/*/; do
    name="$(basename "$dir")"
    [ -f "$dir/Cargo.toml" ] || continue
    CARGO_TARGET_DIR="$root/target/fixtures/$name" cargo build --quiet --manifest-path "$dir/Cargo.toml" "$@"
    echo "$root/target/fixtures/$name/debug/$name"
done
