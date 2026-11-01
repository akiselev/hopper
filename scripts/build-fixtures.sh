#!/usr/bin/env bash
# Build every fixture under fixtures/ (debug, opt-level 0, full debuginfo) into
# target/fixtures/<name>/. Prints the path of each binary.
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
for dir in "$root"/fixtures/*/; do
    name="$(basename "$dir")"
    [ -f "$dir/Cargo.toml" ] || continue
    CARGO_TARGET_DIR="$root/target/fixtures/$name" cargo build --quiet --manifest-path "$dir/Cargo.toml" "$@"
    echo "$root/target/fixtures/$name/debug/$name"
done
