#!/usr/bin/env bash
# E1: can this machine record with rr? Checks the retired-conditional-branch counter rr needs.
source "$(dirname "$0")/lib.sh"
begin_report e1-pmu "E1: does rr work here?"

say "## Machine"
say
fence lscpu || true
fence sh -c 'systemd-detect-virt 2>/dev/null || echo "systemd-detect-virt unavailable"' || true
fence cat /proc/sys/kernel/perf_event_paranoid || true
fence sh -c 'ls /sys/bus/event_source/devices/ 2>&1' || true
if grep -qi 'AuthenticAMD' /proc/cpuinfo; then
    say "AMD CPU: rr needs the SpecLockMap workaround after every boot (rr wiki, Zen)."
    say
fi

say "## perf"
say
perf_ok=no
if command -v perf > /dev/null; then
    if fence perf stat -e br_inst_retired.conditional true; then perf_ok=yes; fi
else
    say "perf is not installed."
    say
fi

say "## rr record"
say
rr_ok=no
if command -v rr > /dev/null; then
    if fence rr record -o "$WORK/true-trace" true; then
        rr_ok=yes
        fence rr replay -a "$WORK/true-trace" || rr_ok=no
    fi
else
    say "rr is not installed."
    say
fi

say "## Result"
say
say "- perf counts br_inst_retired.conditional: **$perf_ok**"
say "- rr records and replays /bin/true: **$rr_ok**"
finish
