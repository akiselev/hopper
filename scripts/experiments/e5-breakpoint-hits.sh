#!/usr/bin/env bash
# E5: replay cost of a phase-A index: a breakpoint hit 10^3, 10^4 and 10^5 times.
source "$(dirname "$0")/lib.sh"
need rr cargo cc
begin_report e5-breakpoint-hits "E5: replay cost per breakpoint hit"

cat > "$WORK/hits.c" <<'C'
#include <stdlib.h>
volatile long sink;
__attribute__((noinline)) void hit(long i) { sink += i; }
int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 1000;
    for (long i = 0; i < n; i++) hit(i);
    return 0;
}
C
cc -O0 -g -o "$WORK/hits" "$WORK/hits.c"
cargo build --quiet --manifest-path "$HOPPER_ROOT/Cargo.toml" -p hopper-rr --example breakpoint_hits --release
example="$CARGO_TARGET_DIR/release/examples/breakpoint_hits"

say "One function \`hit\` called N times; the whole recording is replayed on a fresh server"
say "without breakpoints, then with a breakpoint on \`hit\`, resuming at every hit."
say
say "| N | hits | replay without breakpoints (s) | replay with breakpoint (s) | cost per hit (µs) |"
say "|---|---|---|---|---|"
for n in 1000 10000 100000; do
    trace="$WORK/hits-$n"
    if ! rr record -o "$trace" "$WORK/hits" "$n" > "$WORK/record-$n.log" 2>&1; then
        say "| $n | recording failed: $(tail -1 "$WORK/record-$n.log") | | | |"
        continue
    fi
    row="$("$example" "$trace" "$WORK/hits" hit 2>&1)" || row="| failed: $(echo "$row" | tail -1) | | | |"
    say "| $n $row"
done
finish
