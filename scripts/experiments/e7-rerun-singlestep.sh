#!/usr/bin/env bash
# E7: throughput of `rr rerun --singlestep`, to calibrate the "roughly 1000x" estimate.
source "$(dirname "$0")/lib.sh"
need rr cc bc
begin_report e7-rerun-singlestep "E7: rr rerun --singlestep throughput"

cat > "$WORK/loop.c" <<'C'
#include <stdlib.h>
volatile long sink;
int main(int argc, char **argv) {
    long n = argc > 1 ? atol(argv[1]) : 100000;
    for (long i = 0; i < n; i++) sink += i * 3;
    return 0;
}
C
cc -O0 -g -o "$WORK/loop" "$WORK/loop.c"
trace="$WORK/loop-trace"
if ! fence rr record -o "$trace" "$WORK/loop" 200000; then
    say "Recording failed."
    finish
    exit 0
fi

native=$(seconds "$WORK/loop" 200000)
replay=$(seconds rr replay -a "$trace")
start=$(date +%s.%N)
rr rerun --singlestep=event,icount,ip "$trace" > "$WORK/steps.txt" 2> "$WORK/rerun.err" || true
end=$(date +%s.%N)
rerun=$(echo "$end - $start" | bc)
steps=$(wc -l < "$WORK/steps.txt")

say "| measure | value |"
say "|---|---|"
say "| native run (s) | $native |"
say "| rr replay -a (s) | $replay |"
say "| rr rerun --singlestep (s) | $rerun |"
say "| single steps dumped | $steps |"
if [ "$steps" -gt 0 ]; then
    say "| steps per second | $(echo "$steps / $rerun" | bc) |"
    say "| singlestep slowdown vs replay | $(echo "scale=1; $rerun / $replay" | bc)x |"
    say "| singlestep slowdown vs native | $(echo "scale=1; $rerun / $native" | bc)x |"
fi
say
say "rerun stderr (last lines):"
say
say '```'
tail -n 20 "$WORK/rerun.err" >> "$REPORT"
say '```'
finish
