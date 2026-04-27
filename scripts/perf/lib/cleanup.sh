#!/usr/bin/env bash
# lib/cleanup.sh — targeted cleanup between perf runs.
#
# Kills the load generator, stops RustFS on all nodes, removes only the
# bucket directories created by the load generator (identified by the Bucket
# prefix in the loadgen config file), then restarts RustFS and waits for
# healthy.
#
# This is faster than letting the loadgen run its own S3 DELETE cleanup, and
# more targeted than wiping entire data directories.
#
# Usage (standalone):
#   source conf/paths.env
#   bash scripts/perf/lib/cleanup.sh
#
# Also called by run-perf-test.sh when --force-cleanup is passed.
#
# Required env (sourced from conf/paths.env by the caller):
#   PEER_NODES    space-separated peer hostnames
#   DATA_DIRS     space-separated data directories on every node
#   LOADGEN_CFG   path to the loadgen JSON config file (to read Bucket prefix)
#   LOADGEN_HOST  SSH target for loadgen machine; empty = local

set -euo pipefail

log()  { echo "[cleanup] $(date -u '+%H:%M:%S') $*"; }
die()  { echo "[cleanup] ERROR: $*" >&2; exit 1; }

# Validate required vars before touching anything
: "${PEER_NODES?PEER_NODES must be set (source conf/paths.env)}"
: "${DATA_DIRS?DATA_DIRS must be set (source conf/paths.env)}"
: "${LOADGEN_CFG?LOADGEN_CFG must be set (source conf/paths.env)}"

# ---------------------------------------------------------------------------
# Derive bucket prefix from the loadgen config file
# ---------------------------------------------------------------------------
# The loadgen names buckets as: <Bucket>-<testNum>-<1..N>
# We match <Bucket>-* to catch all runs regardless of test number.

[[ -f "$LOADGEN_CFG" ]] || die "LOADGEN_CFG not found: $LOADGEN_CFG"

BUCKET_PREFIX="$(python3 -c "
import json, sys
with open(sys.argv[1]) as f:
    print(json.load(f)['Bucket'])
" "$LOADGEN_CFG")"

[[ -n "$BUCKET_PREFIX" ]] || die "Could not read Bucket field from $LOADGEN_CFG"
log "Bucket prefix: '${BUCKET_PREFIX}' (will remove '${BUCKET_PREFIX}-*' in each data dir)"

# ---------------------------------------------------------------------------
# 1. KILL LOAD GENERATOR
# ---------------------------------------------------------------------------

log "Killing load generator..."
if [[ -n "${LOADGEN_HOST:-}" ]]; then
    ssh "$LOADGEN_HOST" "pkill -f load-generator || true"
else
    pkill -f load-generator || true
fi
log "Load generator stopped"

# ---------------------------------------------------------------------------
# 2. STOP RUSTFS — peers first, then local
# ---------------------------------------------------------------------------

for node in $PEER_NODES; do
    log "Stopping rustfs service on $node..."
    ssh "$node" "sudo systemctl stop rustfs || true"
done

log "Stopping rustfs service locally..."
sudo systemctl stop rustfs || true

sleep 1

# ---------------------------------------------------------------------------
# 3. REMOVE BUCKET DIRECTORIES — targeted match on prefix, all nodes
# ---------------------------------------------------------------------------

for dir in $DATA_DIRS; do
    : "${dir:?DATA_DIRS contains an empty token — aborting}"

    for node in $PEER_NODES; do
        log "Removing '${BUCKET_PREFIX}-*' from ${dir} on $node..."
        ssh "$node" "
            count=\$(sudo find '${dir}' -maxdepth 1 -type d -name '${BUCKET_PREFIX}-*' | wc -l)
            if [[ \$count -gt 0 ]]; then
                sudo find '${dir}' -maxdepth 1 -type d -name '${BUCKET_PREFIX}-*' | xargs sudo rm -rf
                echo \"  removed \$count bucket dir(s) from ${dir} on \$(hostname)\"
            else
                echo \"  nothing to remove in ${dir} on \$(hostname)\"
            fi
        "
    done

    log "Removing '${BUCKET_PREFIX}-*' from ${dir} locally..."
    count=$(sudo find "${dir}" -maxdepth 1 -type d -name "${BUCKET_PREFIX}-*" | wc -l)
    if [[ "$count" -gt 0 ]]; then
        sudo find "${dir}" -maxdepth 1 -type d -name "${BUCKET_PREFIX}-*" | xargs sudo rm -rf
        log "  removed $count bucket dir(s) from ${dir} locally"
    else
        log "  nothing to remove in ${dir} locally"
    fi
done

# ---------------------------------------------------------------------------
# 4. RESTART RUSTFS — peers first, then local
# ---------------------------------------------------------------------------

for node in $PEER_NODES; do
    log "Starting rustfs service on $node..."
    ssh "$node" "sudo systemctl start rustfs"
done

log "Starting rustfs service locally..."
sudo systemctl start rustfs

# ---------------------------------------------------------------------------
# 5. HEALTH POLL — all nodes must respond before returning
# ---------------------------------------------------------------------------

health_poll() {
    local host="$1"
    local url="http://${host}:9000/minio/health/live"
    local attempts=0
    local max=30

    while (( attempts < max )); do
        if curl -sf --max-time 2 "$url" > /dev/null 2>&1; then
            log "Node $host is healthy"
            return 0
        fi
        (( attempts++ ))
        sleep 1
    done
    die "Node $host did not become healthy after ${max}s"
}

for node in $PEER_NODES; do
    health_poll "$node"
done
health_poll "127.0.0.1"

log "Cleanup complete — all nodes healthy, test buckets removed"
