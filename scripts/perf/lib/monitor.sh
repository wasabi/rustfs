#!/usr/bin/env bash
# lib/monitor.sh — per-node telemetry capture for a perf run.
#
# Takes a T0 NIC snapshot, starts background CPU/disk/network capture, then
# blocks until SIGTERM.  On SIGTERM it stops all background processes, takes
# a T1 NIC snapshot, and exits cleanly.
#
# run-perf-test.sh runs this script:
#   - locally (node1) as a background subshell
#   - on each peer node via: ssh $node "OUT=... NODE_ID=... ... bash monitor.sh &"
#
# After the loadgen finishes, run-perf-test.sh sends SIGTERM to each monitor
# process (local PID + remote via ssh kill).
#
# Environment (required):
#   NODE_ID         integer node number (1 for node1, 2 for node2, etc.)
#   OUT             writable output directory (created if absent)
#
# Environment (optional):
#   PEER_SPEC       HOST:PORT of the peer to trace in ss -ti snapshots
#                   e.g. "node2:9000" on node1, "node1:9000" on node2
#   NIC_INTERFACES  space-separated interface names for ethtool -S snapshots
#                   e.g. "eno1409 eno1419"  (from conf/paths.env)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

: "${NODE_ID:?Set NODE_ID to the node number (e.g. 1 or 2)}"
: "${OUT:?Set OUT to a writable output directory}"

mkdir -p "$OUT"

log() { echo "[monitor:node${NODE_ID}] $(date -u '+%H:%M:%S') $*"; }

# ---------------------------------------------------------------------------
# Build NIC snapshot arg list
# ---------------------------------------------------------------------------
# step3-tcp-nic-snapshot.sh takes: TAG OUT NODE_ID [PEER_SPEC] [NIC ...]
# Pass an empty string for PEER_SPEC position when not set so NIC args stay
# in the right positions.

NIC_ARGS=()
if [[ -n "${NIC_INTERFACES:-}" ]]; then
    read -ra NIC_ARGS <<< "$NIC_INTERFACES"
fi

nic_snapshot() {
    local tag="$1"
    bash "$SCRIPT_DIR/step3-tcp-nic-snapshot.sh" \
        "$tag" "$OUT" "$NODE_ID" \
        "${PEER_SPEC:-}" \
        "${NIC_ARGS[@]+"${NIC_ARGS[@]}"}"
}

# ---------------------------------------------------------------------------
# T0 snapshot — taken before loadgen starts
# ---------------------------------------------------------------------------

log "Taking T0 NIC snapshot..."
nic_snapshot T0

# ---------------------------------------------------------------------------
# Start background capture processes
# ---------------------------------------------------------------------------

# CPU + disk via the existing capture script (handles its own SIGTERM)
log "Starting CPU/disk capture (mpstat + iostat)..."
export OUT NODE_ID
bash "$SCRIPT_DIR/step3-capture-mpstat-iostat.sh" &
CAPTURE_PID=$!

# Per-interface network bandwidth via sar -n DEV
SAR_FILE="$OUT/sar-net-node${NODE_ID}.txt"
log "Starting network capture (sar -n DEV → ${SAR_FILE})..."
{
    date -u '+%Y-%m-%dT%H:%M:%SZ'
    echo "OUT=$OUT NODE_ID=$NODE_ID"
    sar -n DEV 1
} >> "$SAR_FILE" &
SAR_PID=$!

log "Capture running (capture_pid=${CAPTURE_PID} sar_pid=${SAR_PID}) — waiting for SIGTERM"

# ---------------------------------------------------------------------------
# SIGTERM handler — called by run-perf-test.sh when loadgen finishes
# ---------------------------------------------------------------------------

stop_capture() {
    log "SIGTERM received — stopping capture..."

    # Stop mpstat/iostat subprocess (its own trap will kill its children)
    kill "$CAPTURE_PID" 2>/dev/null || true
    wait "$CAPTURE_PID" 2>/dev/null || true

    # Stop sar
    kill "$SAR_PID" 2>/dev/null || true
    wait "$SAR_PID" 2>/dev/null || true

    # T1 snapshot — taken after loadgen stops
    log "Taking T1 NIC snapshot..."
    nic_snapshot T1

    log "Monitor stopped at $(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
        | tee -a "$OUT/monitor-node${NODE_ID}.log"

    exit 0
}

trap stop_capture TERM INT

# Block until signal arrives
wait "$CAPTURE_PID" "$SAR_PID" || true
