#!/usr/bin/env bash
# lib/deploy.sh — build, ship, configure, and restart RustFS on all cluster nodes.
#
# Called by run-perf-test.sh unless --no-deploy is passed.
#
# Required env (sourced from conf/paths.env by the caller):
#   PEER_NODES          space-separated peer hostnames
#   PEER_RUSTFS_BIN     path to rustfs binary on peer nodes (e.g. /usr/local/bin/rustfs)
#   RUSTFS_VOLUMES      full volumes string for this topology
#
# Required env (set by run-perf-test.sh from flags / caller environment):
#   RUST_LOG            error (normal) or debug recipe (--trace mode)
#   RUSTFS_BINARY       path to pre-built binary; if empty, cargo build is run
#
# Output: logs to stdout (captured into run.log by the caller).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PERF_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF_TEMPLATE="$PERF_DIR/conf/rustfs.env.template"
RUSTFS_ENV_FILE="/etc/default/rustfs"

log() { echo "[deploy] $(date -u '+%H:%M:%S') $*"; }
die() { echo "[deploy] ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 1. BUILD (skipped when RUSTFS_BINARY is already set by caller)
# ---------------------------------------------------------------------------

if [[ -z "${RUSTFS_BINARY:-}" ]]; then
    log "Building rustfs release binary..."
    # Navigate to repo root (three levels up from scripts/perf/lib/)
    REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
    cargo build --release -p rustfs -j"$(nproc)" --manifest-path "$REPO_ROOT/Cargo.toml"
    RUSTFS_BINARY="$REPO_ROOT/target/release/rustfs"
    log "Build complete: $RUSTFS_BINARY"
else
    log "Using pre-built binary: $RUSTFS_BINARY"
fi

[[ -f "$RUSTFS_BINARY" ]] || die "Binary not found: $RUSTFS_BINARY"

# ---------------------------------------------------------------------------
# 2. SHIP BINARY TO PEERS
# ---------------------------------------------------------------------------

for node in $PEER_NODES; do
    log "Shipping binary to $node..."
    scp -q "$RUSTFS_BINARY" "$node:/tmp/rustfs.new"
    ssh "$node" "sudo mv /tmp/rustfs.new '${PEER_RUSTFS_BIN}' && sudo chmod +x '${PEER_RUSTFS_BIN}'"
    log "Binary installed on $node"
done

# Install locally as well (node1 runs the same service)
log "Installing binary locally..."
sudo cp "$RUSTFS_BINARY" "${PEER_RUSTFS_BIN}"
sudo chmod +x "${PEER_RUSTFS_BIN}"

# ---------------------------------------------------------------------------
# 3. EXPAND ENV TEMPLATE AND WRITE TO ALL NODES
# ---------------------------------------------------------------------------

expand_template() {
    sed \
        -e "s|@@RUSTFS_VOLUMES@@|${RUSTFS_VOLUMES}|g" \
        -e "s|@@RUST_LOG@@|${RUST_LOG}|g" \
        "$CONF_TEMPLATE"
}

log "Expanding env template..."
EXPANDED="$(expand_template)"

# Write to peers first so they are configured before node1 restarts
for node in $PEER_NODES; do
    log "Writing env file to $node:${RUSTFS_ENV_FILE}..."
    echo "$EXPANDED" | ssh "$node" "sudo tee '${RUSTFS_ENV_FILE}' > /dev/null"
done

# Write locally
log "Writing env file locally..."
echo "$EXPANDED" | sudo tee "$RUSTFS_ENV_FILE" > /dev/null

# ---------------------------------------------------------------------------
# 4. RESTART RUSTFS via systemd — peers first, then local
# ---------------------------------------------------------------------------
# Peers-first avoids the window where node1 restarts and immediately tries to
# reach peers still running the old binary / old env.
# systemd reads the updated /etc/default/rustfs automatically on restart.

for node in $PEER_NODES; do
    log "Restarting rustfs service on $node..."
    ssh "$node" "sudo systemctl restart rustfs"
done

log "Restarting rustfs service locally..."
sudo systemctl restart rustfs

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

log "All nodes healthy — deploy complete"
