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
# Optional env (source conf/paths.env or export before deploy):
#   RUSTFS_CONSOLE_VERSION  Console zip version tag (default: latest); see build-rustfs.sh.
#
# Web UI: rust-embed bundles rustfs/static at compile time (rustfs/src/admin/console.rs).
# If rustfs/static/index.html is missing, we download the official console zip before cargo.
#
# Output: logs to stdout (captured into run.log by the caller).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PERF_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
CONF_TEMPLATE="$PERF_DIR/conf/rustfs.env.template"
RUSTFS_ENV_FILE="/etc/default/rustfs"
MERGE_SCRIPT="$SCRIPT_DIR/merge_rustfs_default.py"

log() { echo "[deploy] $(date -u '+%H:%M:%S') $*"; }
die() { echo "[deploy] ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Console static assets for rust-embed ($CARGO_MANIFEST_DIR/static in rustfs crate).
# Mirrors build-rustfs.sh download_console_assets when console assets are needed.
# ---------------------------------------------------------------------------

ensure_console_static_assets() {
    local repo_root="$1"
    local static_dir="$repo_root/rustfs/static"

    if [[ -f "$static_dir/index.html" ]]; then
        log "Console static assets OK ($static_dir)"
        return 0
    fi

    log "Console static assets missing (no $static_dir/index.html) — downloading..."
    command -v curl >/dev/null 2>&1 || die "curl is required to download console assets"
    command -v unzip >/dev/null 2>&1 || die "unzip is required to extract console assets"

    local ver="${RUSTFS_CONSOLE_VERSION:-latest}"
    local download_url
    if [[ "$ver" == "latest" ]]; then
        download_url="https://dl.rustfs.com/artifacts/console/rustfs-console-latest.zip"
    else
        download_url="https://dl.rustfs.com/artifacts/console/rustfs-console-${ver}.zip"
    fi

    mkdir -p "$static_dir"
    local temp_file
    temp_file="$(mktemp "${TMPDIR:-/tmp}/rustfs-console-XXXXXX.zip")"
    local i
    for i in 1 2 3; do
        if curl -fL "$download_url" -o "$temp_file" --retry 3 --retry-delay 5 --max-time 300; then
            break
        fi
        [[ "$i" -eq 3 ]] && { rm -f "$temp_file"; die "Failed to download console assets from $download_url"; }
        log "Download attempt $i failed, retrying..."
        sleep 2
    done

    [[ -s "$temp_file" ]] || { rm -f "$temp_file"; die "Downloaded console zip is empty"; }

    if unzip -o "$temp_file" -d "$static_dir"; then
        rm -f "$temp_file"
        [[ -f "$static_dir/index.html" ]] || die "Console zip did not produce $static_dir/index.html"
        log "Console assets installed ($(du -sh "$static_dir" 2>/dev/null | awk '{print $1}'))"
    else
        rm -f "$temp_file"
        die "Failed to extract console zip"
    fi
}

# ---------------------------------------------------------------------------
# 1. BUILD (skipped when RUSTFS_BINARY is already set by caller)
# ---------------------------------------------------------------------------

if [[ -z "${RUSTFS_BINARY:-}" ]]; then
    log "Building rustfs release binary..."
    # Navigate to repo root (three levels up from scripts/perf/lib/)
    REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
    ensure_console_static_assets "$REPO_ROOT"
    # cd into REPO_ROOT so cargo picks up .cargo/config.toml (tokio_unstable flag).
    # --manifest-path alone does not affect the config lookup, which walks up from CWD.
    (cd "$REPO_ROOT" && cargo build --release -p rustfs -j"$(nproc)")
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

# Install locally as well (node1 runs the same service).
# Use /tmp then mv — in-place cp fails with "Text file busy" while rustfs is running.
log "Installing binary locally..."
sudo cp "$RUSTFS_BINARY" /tmp/rustfs.new
sudo mv /tmp/rustfs.new "${PEER_RUSTFS_BIN}"
sudo chmod +x "${PEER_RUSTFS_BIN}"

# ---------------------------------------------------------------------------
# 3. EXPAND ENV TEMPLATE, MERGE WITH EXISTING /etc/default/rustfs, WRITE ALL NODES
# ---------------------------------------------------------------------------
# Template wins for any KEY= also present with a different value. Assignments whose KEY
# is not mentioned in rustfs.env.template are preserved verbatim from the existing file
# (standalone comments/blanks outside the appended block are not preserved — keep it simple).

expand_template() {
    sed \
        -e "s|@@RUSTFS_VOLUMES@@|${RUSTFS_VOLUMES}|g" \
        -e "s|@@RUST_LOG@@|${RUST_LOG}|g" \
        "$CONF_TEMPLATE"
}

merge_env_file_to_temp() {
    local existing_tmp="$1"
    [[ -f "$MERGE_SCRIPT" ]] || die "missing merge helper: $MERGE_SCRIPT"
    printf '%s' "$EXPANDED" | python3 "$MERGE_SCRIPT" "$existing_tmp"
}

log "Expanding env template..."
EXPANDED="$(expand_template)"

tmp_existing="$(mktemp)"
trap 'rm -f "$tmp_existing"' EXIT

# Write to peers first so they are configured before node1 restarts
for node in $PEER_NODES; do
    log "Writing env file to $node:${RUSTFS_ENV_FILE} (merge with existing)..."
    : >"$tmp_existing"
    ssh "$node" "sudo cat '${RUSTFS_ENV_FILE}'" 2>/dev/null >>"$tmp_existing" || true
    merge_env_file_to_temp "$tmp_existing" | ssh "$node" "sudo tee '${RUSTFS_ENV_FILE}' > /dev/null"
done

# Write locally
log "Writing env file locally (merge with existing)..."
: >"$tmp_existing"
sudo cat "$RUSTFS_ENV_FILE" 2>/dev/null >>"$tmp_existing" || true
merge_env_file_to_temp "$tmp_existing" | sudo tee "$RUSTFS_ENV_FILE" > /dev/null

trap - EXIT
rm -f "$tmp_existing"

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
    local ssh_target="$1"
    # Strip user@ for HTTP URLs (curl does not accept ba@host:port like SSH does).
    local host="${ssh_target##*@}"
    # Liveness: GET /health → 200 (RustFS). Do not use MinIO's /minio/health/live — that path
    # is not the RustFS admin health route and falls through to S3 (AccessDenied).
    local url="http://${host}:9000/health"
    local attempts=0
    local max=30

    while (( attempts < max )); do
        if curl -sf --max-time 2 "$url" > /dev/null 2>&1; then
            log "Node $host is healthy"
            return 0
        fi
        # Not ((attempts++)) — with set -e, first increment from 0 exits the script.
        attempts=$((attempts + 1))
        sleep 1
    done
    die "Node $ssh_target ($host) did not become healthy after ${max}s"
}

for node in $PEER_NODES; do
    health_poll "$node"
done
health_poll "127.0.0.1"

log "All nodes healthy — deploy complete"
