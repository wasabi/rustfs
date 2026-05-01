#!/usr/bin/env bash
# lib/cleanup.sh — targeted cleanup between perf runs.
#
# Kills the load generator, stops RustFS on all nodes, removes only the
# bucket directories created by the load generator (identified by the Bucket
# prefix in the loadgen config file), then restarts RustFS and waits for
# healthy. Stop, rm, start, and health polling run in parallel across nodes
# and data dirs (barrier wait between phases).
#
# This is faster than letting the loadgen run its own S3 DELETE cleanup, and
# more targeted than wiping entire data directories.
#
# Usage (standalone):
#   source conf/paths.env
#   bash scripts/perf/lib/cleanup.sh
#
# Called from run-perf-test.sh when --force-cleanup, after PUT traffic (/AVG), to
# terminate loadgen early and skip the slower loadgen DELETE phase — same rm/restart logic.
#
# Required env (sourced from conf/paths.env by the caller):
#   PEER_NODES    space-separated peer hostnames (optional; empty = single-node, local only)
#   DATA_DIRS     space-separated data directories on every node
#   LOADGEN_CFG   path to the loadgen JSON config (to read Bucket prefix). When
#                 LOADGEN_HOST is set, this path is on the loadgen host; otherwise local.
#   LOADGEN_HOST  SSH target for loadgen machine; empty = local

set -euo pipefail

log()  { printf '%s\n' "[cleanup] $(date -u '+%H:%M:%S') $*"; }
die()  { echo "[cleanup] ERROR: $*" >&2; exit 1; }

# Validate required vars before touching anything (PEER_NODES may be unset for single-node)
: "${DATA_DIRS?DATA_DIRS must be set (source conf/paths.env)}"
: "${LOADGEN_CFG?LOADGEN_CFG must be set (source conf/paths.env)}"

PEER_NODES="${PEER_NODES:-}"

# ---------------------------------------------------------------------------
# Derive bucket prefix from the loadgen config file
# ---------------------------------------------------------------------------
# The loadgen names buckets as: <Bucket>-<testNum>-<1..N>
# We match <Bucket>-* to catch all runs regardless of test number.
#
# LOADGEN_CFG is interpreted like loadgen-run.sh: a path on LOADGEN_HOST when
# remote; a local path when LOADGEN_HOST is empty.

if [[ -n "${LOADGEN_HOST:-}" ]]; then
    log "Reading Bucket prefix from ${LOADGEN_CFG} on ${LOADGEN_HOST}..."
    _cfg_q="$(printf '%q' "$LOADGEN_CFG")"
    if ! ssh "$LOADGEN_HOST" "test -f ${_cfg_q}"; then
        die "LOADGEN_CFG not found on ${LOADGEN_HOST}: $LOADGEN_CFG"
    fi
    BUCKET_PREFIX="$(
        ssh "$LOADGEN_HOST" "cat -- ${_cfg_q}" | python3 -c "
import json, sys
print(json.load(sys.stdin)['Bucket'])
"
    )" || die "Failed to read Bucket from remote ${LOADGEN_HOST}:$LOADGEN_CFG"
else
    [[ -f "$LOADGEN_CFG" ]] || die "LOADGEN_CFG not found: $LOADGEN_CFG"
    BUCKET_PREFIX="$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    print(json.load(f)['Bucket'])
" "$LOADGEN_CFG")"
fi

[[ -n "$BUCKET_PREFIX" ]] || die "Could not read Bucket field from $LOADGEN_CFG"
log "Bucket prefix: '${BUCKET_PREFIX}' (will remove '${BUCKET_PREFIX}-*' in each data dir)"

# ---------------------------------------------------------------------------
# 1. KILL LOAD GENERATOR
# ---------------------------------------------------------------------------

log "Killing load generator..."
# Pattern must not match this pkill command line (see ssh … "pkill -f …"), or pkill can
# terminate its own remote shell and ssh exits 255. Bracket trick: only real binary paths
# match, not the literal substring in argv.
if [[ -n "${LOADGEN_HOST:-}" ]]; then
    ssh "$LOADGEN_HOST" "pkill -f '[l]oad-generator' || true"
else
    pkill -f '[l]oad-generator' || true
fi
log "Load generator stopped"

# ---------------------------------------------------------------------------
# 2. STOP RUSTFS — all peers and local in parallel, then barrier wait
# ---------------------------------------------------------------------------

log "Stopping rustfs on all nodes (parallel)..."

STOP_PIDS=()
for node in $PEER_NODES; do
    log "Stopping rustfs on $node..."
    ( ssh "$node" "sudo systemctl stop rustfs || true" ) &
    STOP_PIDS+=($!)
done

log "Stopping rustfs locally..."
( sudo systemctl stop rustfs || true ) &
STOP_PIDS+=($!)

for pid in "${STOP_PIDS[@]}"; do
    wait "$pid" || true
done

sleep 1

# ---------------------------------------------------------------------------
# 3. REMOVE BUCKET DIRECTORIES — each (node × data dir) concurrently
# ---------------------------------------------------------------------------

log "Removing '${BUCKET_PREFIX}-*' across nodes and data dirs (parallel)..."

RM_PIDS=()
for dir in $DATA_DIRS; do
    : "${dir:?DATA_DIRS contains an empty token — aborting}"

    for node in $PEER_NODES; do
        log "Removing '${BUCKET_PREFIX}-*' from ${dir} on $node..."
        (
            ssh "$node" "
                count=\$(sudo find '${dir}' -maxdepth 1 -type d -name '${BUCKET_PREFIX}-*' | wc -l)
                if [[ \$count -gt 0 ]]; then
                    sudo find '${dir}' -maxdepth 1 -type d -name '${BUCKET_PREFIX}-*' | xargs sudo rm -rf
                    echo \"  removed \$count bucket dir(s) from ${dir} on \$(hostname)\"
                else
                    echo \"  nothing to remove in ${dir} on \$(hostname)\"
                fi
            "
        ) &
        RM_PIDS+=($!)
    done

    log "Removing '${BUCKET_PREFIX}-*' from ${dir} locally..."
    (
        count=$(sudo find "${dir}" -maxdepth 1 -type d -name "${BUCKET_PREFIX}-*" | wc -l)
        if [[ "$count" -gt 0 ]]; then
            sudo find "${dir}" -maxdepth 1 -type d -name "${BUCKET_PREFIX}-*" | xargs sudo rm -rf
            log "  removed $count bucket dir(s) from ${dir} locally"
        else
            log "  nothing to remove in ${dir} locally"
        fi
    ) &
    RM_PIDS+=($!)
done

for pid in "${RM_PIDS[@]}"; do
    wait "$pid" || die "Bucket removal failed (pid=$pid)"
done

# ---------------------------------------------------------------------------
# 4. RESTART RUSTFS — peers and local in parallel, then barrier wait
# ---------------------------------------------------------------------------

log "Starting rustfs on all nodes (parallel)..."

START_PIDS=()
for node in $PEER_NODES; do
    log "Starting rustfs on $node..."
    (
        ssh "$node" "sudo systemctl start rustfs"
        log "Started rustfs on $node"
    ) &
    START_PIDS+=($!)
done

log "Starting rustfs locally..."
(
    sudo systemctl start rustfs
    log "Started rustfs locally"
) &
START_PIDS+=($!)

for pid in "${START_PIDS[@]}"; do
    wait "$pid" || die "systemctl start rustfs failed (pid=$pid)"
done

# ---------------------------------------------------------------------------
# 5. HEALTH POLL — poll all nodes concurrently; all must pass
# ---------------------------------------------------------------------------

health_poll() {
    local ssh_target="$1"
    local host="${ssh_target##*@}"
    local url="http://${host}:9000/health"
    local attempts=0
    local max=30

    while (( attempts < max )); do
        if curl -sf --max-time 2 "$url" > /dev/null 2>&1; then
            log "Node $host is healthy"
            return 0
        fi
        attempts=$((attempts + 1))
        sleep 1
    done
    die "Node $ssh_target ($host) did not become healthy after ${max}s"
}

POLL_PIDS=()
for node in $PEER_NODES; do
    log "Polling health on $node..."
    health_poll "$node" &
    POLL_PIDS+=($!)
done
log "Polling health on 127.0.0.1..."
health_poll "127.0.0.1" &
POLL_PIDS+=($!)

for pid in "${POLL_PIDS[@]}"; do
    wait "$pid" || die "Health check failed (pid=$pid)"
done

log "Cleanup complete — all nodes healthy, test buckets removed"
