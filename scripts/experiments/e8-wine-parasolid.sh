#!/usr/bin/env bash
# E8: record a parasolid-rs Wine test under rr.
# Usage: e8-wine-parasolid.sh <test.exe> [args...]
# The exe and pskernel.dll must be findable by Wine (copy the DLL next to the exe, as
# parasolid-rs's CLAUDE.md says). The recording stays on this machine: it holds Parasolid DLL
# memory, which must not leave the user's machines.
source "$(dirname "$0")/lib.sh"
need rr wine
if [ $# -lt 1 ]; then
    echo "usage: $0 <test.exe> [args...]" >&2
    exit 2
fi
exe="$1"
shift
begin_report e8-wine-parasolid "E8: recording a parasolid-rs Wine test under rr"
say "- Wine: $(wine --version 2>/dev/null || echo unknown)"
say "- Test executable: \`$exe\` (sha256 $(sha256sum "$exe" | cut -d' ' -f1))"
say

trace="$WORK/wine-trace"
say "## Native run under Wine"
say
native_start=$(date +%s.%N)
fence env WINEDEBUG=-all wine "$exe" "$@" || true
native_end=$(date +%s.%N)

say "## rr record (WINELOADERNOEXEC=1)"
say
record_start=$(date +%s.%N)
recorded=no
if fence env WINEDEBUG=-all WINELOADERNOEXEC=1 rr record -o "$trace" wine "$exe" "$@"; then
    recorded=yes
fi
record_end=$(date +%s.%N)
if grep -q 'partial write' "$REPORT"; then
    say "The 'partial write' error appeared: this Wine lacks MR 7669 (see the plan, E8)."
    say
fi

replayed=no
if [ -d "$trace" ]; then
    say "## Processes in the trace"
    say
    fence rr ps "$trace" || true
    say "## rr replay -a"
    say
    if fence rr replay -a "$trace"; then replayed=yes; fi
fi

say "## Result"
say
say "- native wall time (s): $(echo "$native_end - $native_start" | bc)"
say "- record wall time (s): $(echo "$record_end - $record_start" | bc)"
say "- recorded: **$recorded**; replayed: **$replayed**"
say "- trace: \`$trace\` (kept local; do not copy it off this machine)"
finish
