#!/usr/bin/env bash
# mperf_csv.sh — run mperf-stat once and append its counters to a CSV.
#
#   scripts/mperf_csv.sh counters.csv "warm" EVENTS [OPS] -- ./bench --warm
#   scripts/mperf_csv.sh counters.csv "cold" EVENTS [OPS] -- ./bench --cold
#
# EVENTS is the comma-separated list mperf-stat takes (aliases or raw
# mnemonics). OPS, when given, is the number of operations the run made,
# and each row also gets the count per operation. Rows are
#
#   name,event,count,per_op
#
# with a header when the file is new, so several runs share one file by
# name, the way the latency histogram CSV does. mperf-stat needs sudo.
#
# The mperf-stat binary is $MPERF_STAT, or ~/projects/mperf/mperf-stat.
set -euo pipefail

if [ $# -lt 4 ]; then
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi

csv=$1; name=$2; events=$3; shift 3
ops=""
if [ "$1" != "--" ]; then ops=$1; shift; fi
[ "$1" = "--" ] || { echo "expected -- before the command" >&2; exit 2; }
shift

mperf=${MPERF_STAT:-$HOME/projects/mperf/mperf-stat}
[ -x "$mperf" ] || { echo "mperf-stat not found at $mperf (set MPERF_STAT)" >&2; exit 1; }

report=$(mktemp -t mperf.XXXXXX.json)
trap 'rm -f "$report"' EXIT

# The measured program keeps stdout; the JSON report goes to the file.
sudo "$mperf" -j -e "$events" -o "$report" -- "$@"

python3 - "$csv" "$name" "$report" "$ops" <<'EOF'
import csv, json, os, sys
path, name, report, ops = sys.argv[1:5]
j = json.load(open(report))
new = not os.path.exists(path) or os.path.getsize(path) == 0
with open(path, "a", newline="") as f:
    w = csv.writer(f)
    if new:
        w.writerow(["name", "event", "count", "per_op"])
    for event, count in j["counters"].items():
        per_op = f"{count / float(ops):.4f}" if ops else ""
        w.writerow([name, event, count, per_op])
    t = j.get("time", {})
    if "wall_ns" in t:
        w.writerow([name, "wall_ns", t["wall_ns"], f"{t['wall_ns'] / float(ops):.1f}" if ops else ""])
print(f"appended {len(j['counters'])} counters for '{name}' to {path}")
EOF
