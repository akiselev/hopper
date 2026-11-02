#!/usr/bin/env bash
# E2: latency of rr's RSP seek, bc, bs and when-ticks over hopper's client.
# Usage: e2-rsp-latency.sh [samples]
source "$(dirname "$0")/lib.sh"
need rr cargo
samples="${1:-20}"
begin_report e2-rsp-latency "E2: RSP operation latency on rr replay"

"$HOPPER_ROOT/scripts/build-fixtures.sh" > /dev/null
cargo build --quiet --manifest-path "$HOPPER_ROOT/Cargo.toml" -p hopper-rr --example rsp_latency --release
example="$CARGO_TARGET_DIR/release/examples/rsp_latency"

for fixture in worked-example threads; do
    exe="$HOPPER_ROOT/target/fixtures/$fixture/debug/$fixture"
    trace="$WORK/$fixture-trace"
    say "## $fixture"
    say
    if ! fence rr record -o "$trace" "$exe"; then
        say "Recording failed; see above."
        continue
    fi
    "$example" "$trace" "$samples" >> "$REPORT" 2>&1 || say "rsp_latency failed (exit $?)"
    say
done
say "Samples per operation: $samples (reverse continues: at most 5). Times are wall-clock on"
say "this machine and include the TCP round trip to the local server."
finish
