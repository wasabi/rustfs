#!/usr/bin/env bash
# Stream mpstat + iostat to files under OUT until SIGINT/SIGTERM.
# Called by monitor.sh (or standalone) to capture per-node CPU and disk telemetry.
#
# Usage:
#   export OUT=/path/to/dir NODE_ID=1
#   bash step3-capture-mpstat-iostat.sh
#
# Environment:
#   OUT      — writable output directory (required)
#   NODE_ID  — integer node identifier used in output filenames (required)
set -euo pipefail

: "${OUT:?Set OUT to a writable directory}"
: "${NODE_ID:?Set NODE_ID to the node number (e.g. 1 or 2)}"

mkdir -p "$OUT"
MP="$OUT/mpstat-node${NODE_ID}.txt"
IO="$OUT/iostat-node${NODE_ID}.txt"

{
  date -u '+%Y-%m-%dT%H:%M:%SZ'
  echo "OUT=$OUT NODE_ID=$NODE_ID"
} | tee -a "$MP" | tee -a "$IO" >/dev/null

mpstat -P ALL 1 >> "$MP" &
MP_PID=$!

iostat -xz 1 >> "$IO" &
IO_PID=$!

cleanup() {
  kill "$MP_PID" "$IO_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

wait "$MP_PID" "$IO_PID" || true
