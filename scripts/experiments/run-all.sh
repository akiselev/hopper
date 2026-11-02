#!/usr/bin/env bash
# Run E1, E2, E5 and E7 (E8 needs a test executable: run it by hand).
set -uo pipefail
here="$(dirname "$0")"
"$here/e1-pmu.sh"
"$here/e2-rsp-latency.sh"
"$here/e5-breakpoint-hits.sh"
"$here/e7-rerun-singlestep.sh"
