# Shared helpers for the M0 experiment scripts. Source it; do not run it.
#
# Each experiment writes one Markdown report to docs/experiments/results/<name>.md with a
# header naming the host, so results from several machines can sit side by side (rename or
# copy the file per machine; the scripts overwrite their own file).

set -euo pipefail

HOPPER_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RESULTS_DIR="$HOPPER_ROOT/docs/experiments/results"
WORK_ROOT="${HOPPER_EXPERIMENT_WORK:-$HOPPER_ROOT/target/experiments}"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$HOPPER_ROOT/target}"
export CARGO_TARGET_DIR
mkdir -p "$RESULTS_DIR" "$WORK_ROOT"

# begin_report <name> <title>: start the report file and set $REPORT and $WORK.
begin_report() {
    REPORT="$RESULTS_DIR/$1.md"
    WORK="$WORK_ROOT/$1"
    rm -rf "$WORK"
    mkdir -p "$WORK"
    {
        echo "# $2"
        echo
        echo "- Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
        echo "- Host: $(hostname)"
        echo "- Kernel: $(uname -r)"
        echo "- CPU: $(cpu_model)"
        echo "- rr: $(rr --version 2>/dev/null | head -1 || echo 'not installed')"
        echo "- hopper: $(git -C "$HOPPER_ROOT" rev-parse --short HEAD 2>/dev/null || echo 'not a git checkout')"
        echo
    } > "$REPORT"
}

cpu_model() {
    grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ *//' || echo unknown
}

# say <text>: append a line to the report.
say() {
    echo "$*" >> "$REPORT"
}

# fence <command...>: run a command, append its output (stdout and stderr, last 60 lines) in a
# code block, and return its status without stopping the script.
fence() {
    local status=0
    local out
    out="$("$@" 2>&1)" || status=$?
    {
        echo '```'
        echo "\$ $*"
        echo "$out" | tail -n 60
        echo "(exit status $status)"
        echo '```'
        echo
    } >> "$REPORT"
    return "$status"
}

# seconds <command...>: run a command quietly and print its wall-clock seconds.
seconds() {
    local start end
    start=$(date +%s.%N)
    "$@" > /dev/null 2>&1
    end=$(date +%s.%N)
    echo "$end - $start" | bc
}

need() {
    for tool in "$@"; do
        command -v "$tool" > /dev/null || { echo "missing tool: $tool" >&2; exit 2; }
    done
}

finish() {
    echo "wrote $REPORT"
}
