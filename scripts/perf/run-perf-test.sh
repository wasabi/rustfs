#!/usr/bin/env bash
# run-perf-test.sh — top-level driver for a RustFS performance test run.
#
# Sequences: deploy → monitors → loadgen → stop monitors after PUT →
#            (--force-cleanup: kill loadgen, run cleanup.sh, skip S3 DELETE) →
#            collect peer artifacts → analyze
#
# Usage:
#   source conf/paths.env           # load lab config
#   bash scripts/perf/run-perf-test.sh \
#       --duration 5m \
#       --out /tmp/perf-results
#
# Flags:
#   --duration DURATION             passed to loadgen -dur (default: 5m)
#   --out      DIR                  output root; default: ./perf-results/<UTC-timestamp>
#   --no-deploy                     skip build + restart; nodes must already be running
#   --binary   PATH                 use pre-built binary (skips cargo build; implies deploy)
#   --force-cleanup                 after PUT traffic (see /AVG in loadgen.txt):
#                                   kill loadgen, run cleanup.sh — skip loadgen S3 DELETE,
#                                   remove bucket dirs on disk, restart rustfs
#   --trace                         enable debug RUST_LOG recipe on node1; collect obs JSONL
#
# Required env (source conf/paths.env before running):
#   LOADGEN_BIN, LOADGEN_CFG
# Optional env:
#   PEER_NODES          space-separated peer hostnames; if empty, peer monitoring
#                       and artifact collection are skipped (node1-only run)
#   RUSTFS_VOLUMES      informational; recorded in meta.json
#   DATA_DIRS, PEER_RUSTFS_BIN  used only by deploy.sh / cleanup.sh
#   LOADGEN_ENDPOINT (optional), LOADGEN_HOST (optional)
#   NIC_INTERFACES (optional, for ethtool snapshots)
#   RUSTFS_OBS_LOG_DIR (optional, default /var/logs/rustfs — used by --trace)
#   PUT_PHASE_DRAIN_SECS (optional) extra seconds after loadgen.txt has summary + interval
#                       rows before stopping monitors / killing loadgen; default 0 (often not needed)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
LIB="$SCRIPT_DIR/lib"
ANALYZE="$SCRIPT_DIR/analyze/analyze.py"
BASELINE="$SCRIPT_DIR/benchmarks/baseline.json"
PATHS_ENV="$(cd "$SCRIPT_DIR/../../.." && pwd)/conf/paths.env"

# ---------------------------------------------------------------------------
# Source lab config if not already in environment
# ---------------------------------------------------------------------------

if [[ -f "$PATHS_ENV" ]]; then
    # shellcheck source=/dev/null
    source "$PATHS_ENV"
fi

log()  { echo "[run-perf-test] $(date -u '+%H:%M:%S') $*"; }
die()  { echo "[run-perf-test] ERROR: $*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Parse flags
# ---------------------------------------------------------------------------

DURATION="5m"
OUT=""
NO_DEPLOY=false
RUSTFS_BINARY=""
FORCE_CLEANUP=false
TRACE=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --duration)   DURATION="$2";       shift 2 ;;
        --out)        OUT="$2";            shift 2 ;;
        --no-deploy)  NO_DEPLOY=true;      shift   ;;
        --binary)     RUSTFS_BINARY="$2";  shift 2 ;;
        --force-cleanup) FORCE_CLEANUP=true; shift  ;;
        --trace)      TRACE=true;          shift   ;;
        *) die "Unknown flag: $1" ;;
    esac
done

# Default output dir: timestamped under ./perf-results/
if [[ -z "$OUT" ]]; then
    OUT="$(pwd)/perf-results/$(date -u '+%Y%m%dT%H%M%SZ')"
fi
mkdir -p "$OUT"

# Redirect all subsequent log output to run.log (tee keeps it on stdout too)
exec > >(tee -a "$OUT/run.log") 2>&1

log "=== run-perf-test.sh start ==="
log "duration=${DURATION} out=${OUT}"
log "no_deploy=${NO_DEPLOY} force_cleanup=${FORCE_CLEANUP} trace=${TRACE}"

# ---------------------------------------------------------------------------
# Validate required env
# ---------------------------------------------------------------------------

: "${LOADGEN_BIN:?LOADGEN_BIN must be set (source conf/paths.env)}"
: "${LOADGEN_CFG:?LOADGEN_CFG must be set (source conf/paths.env)}"

if [[ -z "${PEER_NODES:-}" ]]; then
    log "PEER_NODES not set — running node1-only (no peer monitors, no peer artifact collection)"
fi

# ---------------------------------------------------------------------------
# RUST_LOG recipe
# ---------------------------------------------------------------------------

if $TRACE; then
    RUST_LOG='error,rustfs_put_trace=debug,rustfs_lock_rpc=debug,h2=warn,hyper=warn,tonic=warn,reqwest=warn,tower=warn'
    log "Trace mode enabled — RUST_LOG set to debug recipe"
else
    RUST_LOG='error'
fi

# ---------------------------------------------------------------------------
# Export vars consumed by lib scripts
# ---------------------------------------------------------------------------

export PEER_NODES RUSTFS_VOLUMES PEER_RUSTFS_BIN DATA_DIRS
export LOADGEN_BIN LOADGEN_CFG LOADGEN_ENDPOINT LOADGEN_HOST
export LOADGEN_HOST NIC_INTERFACES
export RUST_LOG
export RUSTFS_BINARY
export DURATION
export OUT

# ---------------------------------------------------------------------------
# Track monitor PIDs for the EXIT trap
# ---------------------------------------------------------------------------

LOCAL_MONITOR_PID=""
declare -A REMOTE_MONITOR_PIDS   # node → SSH background PID

stop_monitors() {
    log "Stopping monitors..."
    if [[ -n "$LOCAL_MONITOR_PID" ]]; then
        kill "$LOCAL_MONITOR_PID" 2>/dev/null || true
        wait "$LOCAL_MONITOR_PID" 2>/dev/null || true
    fi
    for node in "${!REMOTE_MONITOR_PIDS[@]}"; do
        local_ssh_pid="${REMOTE_MONITOR_PIDS[$node]}"
        local node_host="${node##*@}"
        local pid_file="${OUT}/node-${node_host}/monitor-node${node_host}-pid.txt"
        # Signal the remote monitor process via its saved PID file
        if ssh "$node" "test -f '${pid_file}'"; then
            remote_pid=$(ssh "$node" "cat '${pid_file}'")
            ssh "$node" "kill '$remote_pid' 2>/dev/null || true" || true
        fi
        # Also kill the local SSH background job
        kill "$local_ssh_pid" 2>/dev/null || true
        wait "$local_ssh_pid" 2>/dev/null || true
    done
}

ARTIFACTS_COLLECTED=false

collect_peer_artifacts() {
    [[ -n "${PEER_NODES:-}" ]] || return 0
    $ARTIFACTS_COLLECTED && return 0
    ARTIFACTS_COLLECTED=true
    log "Collecting artifacts from peer nodes..."
    for node in $PEER_NODES; do
        local node_host="${node##*@}"
        local node_out="$OUT/node-${node_host}"
        log "  scp ${node}:${node_out}/ → ${node_out}/"
        # Copy the peer's subdirectory directly (avoids the "/." suffix that breaks
        # newer OpenSSH and the "@" character that appears when PEER_NODES uses user@host).
        scp -r -q "${node}:${node_out}" "${OUT}/" || log "  WARNING: scp from $node failed"
    done
}

# True when loadgen printed the PUT summary row containing #<test>/ AVG (may follow
# a HH:MM:SS timestamp on the same line — do not anchor to line start).
_loadgen_put_summary_ready() {
    local logfile="$1"
    [[ -f "$logfile" ]] || return 1
    grep -qE '#[[:digit:]]+/[[:space:]]*AVG\b' "$logfile" 2>/dev/null
}

# True when at least one per-interval PUT row exists (#<test>/<n> PUT:, not …/AVG).
# Matches analyze.py — avoids killing the loadgen | tee pipeline before intervals hit disk.
_loadgen_has_put_interval_line() {
    local logfile="$1"
    [[ -f "$logfile" ]] || return 1
    grep -qE '#[[:digit:]]+/[[:space:]]*[0-9]+[[:space:]]+PUT:' "$logfile" 2>/dev/null
}

# EXIT trap: always stop monitors and collect artifacts, even on failure
on_exit() {
    local exit_code=$?
    if [[ $exit_code -ne 0 ]]; then
        log "Exiting with error (code=${exit_code}) — running cleanup..."
    fi
    stop_monitors
    collect_peer_artifacts
    log "=== run-perf-test.sh done (exit_code=${exit_code}) ==="
}
trap on_exit EXIT

# ---------------------------------------------------------------------------
# Step 1 — Deploy
# ---------------------------------------------------------------------------

if ! $NO_DEPLOY; then
    log "--- Step 1: deploy ---"
    bash "$LIB/deploy.sh"
else
    log "--- Step 1: deploy skipped (--no-deploy) ---"
fi

# ---------------------------------------------------------------------------
# Step 2 — Write meta.json
# ---------------------------------------------------------------------------

log "--- Step 2: write meta.json ---"
GIT_SHA="$(git -C "$SCRIPT_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)"
python3 - <<EOF
import json, os
meta = {
    "git_sha": "$GIT_SHA",
    "topology": "${TOPOLOGY_LABEL:-unknown}",
    "duration": "$DURATION",
    "trace": $( $TRACE && echo True || echo False ),
    "utc": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
    "peer_nodes": "${PEER_NODES:-}".split(),
    "nic_interfaces": "$NIC_INTERFACES".split() if "$NIC_INTERFACES" else [],
}
with open("$OUT/meta.json", "w") as f:
    json.dump(meta, f, indent=2)
print("meta.json written")
EOF

# ---------------------------------------------------------------------------
# Step 3 — Start monitors
# ---------------------------------------------------------------------------

log "--- Step 3: start monitors ---"

# Assign NODE_IDs: node1 (local) = 1, peers in PEER_NODES order = 2, 3, ...
NODE_ID=1
mkdir -p "$OUT/node1"

# PEER_SPEC: tcp-socket tracking target for node1's ss snapshots.
# Empty when PEER_NODES is not set (node1-only run).
LOCAL_PEER_SPEC="${PEER_NODES:+${PEER_NODES%% *}:9000}"

NODE_ID=1 OUT="$OUT/node1" PEER_SPEC="${LOCAL_PEER_SPEC}" \
    bash "$LIB/monitor.sh" >> "$OUT/node1/monitor-node1.log" 2>&1 &
LOCAL_MONITOR_PID=$!
log "Local monitor started (pid=${LOCAL_MONITOR_PID})"

if [[ -n "${PEER_NODES:-}" ]]; then
    PEER_NODE_ID=2
    for node in $PEER_NODES; do
        # Strip optional user prefix (e.g. "ba@node2" → "node2") for directory and file names.
        node_host="${node##*@}"
        peer_out="$OUT/node-${node_host}"

        # Ship the lib scripts to the peer so the remote monitor can run even if the
        # peer node does not have a local copy of the repo.
        REMOTE_LIB="/tmp/perf-lib-${node_host}"
        log "Shipping monitor scripts to $node:${REMOTE_LIB}..."
        ssh "$node" "mkdir -p '${REMOTE_LIB}'"
        scp -q "$LIB/"*.sh "$node:${REMOTE_LIB}/"

        # Start monitor on peer; save its PID to a file on the remote for clean signalling.
        # Note: do NOT mkdir "$peer_out" locally here — collect_peer_artifacts uses
        # "scp -r host:$peer_out $OUT/" which would double-nest if the dir already exists.
        ssh "$node" "
            mkdir -p '${peer_out}'
            export NODE_ID=${PEER_NODE_ID}
            export OUT='${peer_out}'
            export PEER_SPEC=''
            export NIC_INTERFACES='${NIC_INTERFACES:-}'
            nohup bash '${REMOTE_LIB}/monitor.sh' \
                >>'${peer_out}/monitor-node${PEER_NODE_ID}.log' 2>&1 &
            echo \$! > '${peer_out}/monitor-node${node_host}-pid.txt'
            disown
        " &
        REMOTE_MONITOR_PIDS["$node"]=$!
        log "Remote monitor started on $node (node_id=${PEER_NODE_ID})"
        (( PEER_NODE_ID++ ))
    done
fi

# Brief pause to let monitors take their T0 snapshots before load starts
sleep 2

# ---------------------------------------------------------------------------
# Step 4 — Run load generator
# ---------------------------------------------------------------------------

log "--- Step 4: run loadgen (duration=${DURATION}) ---"
bash "$LIB/loadgen-run.sh" &
LOADGEN_PID=$!

# Stop monitors after PUT traffic ends — wait for #<test>/AVG, then until a per-interval
# PUT row appears in loadgen.txt (tee can lag); optional extra drain (see PUT_PHASE_DRAIN_SECS).
LOADGEN_OUT="$OUT/loadgen.txt"
LOADGEN_FORCE_KILLED=false
PUT_PHASE_DRAIN_SECS="${PUT_PHASE_DRAIN_SECS:-0}"

while kill -0 "$LOADGEN_PID" 2>/dev/null \
      && ! _loadgen_put_summary_ready "$LOADGEN_OUT"; do
    sleep 1
done

if _loadgen_put_summary_ready "$LOADGEN_OUT"; then
    log "PUT summary (#…/ AVG) seen — waiting for per-interval PUT rows in ${LOADGEN_OUT##*/}"
    spins=0
    while kill -0 "$LOADGEN_PID" 2>/dev/null \
          && ! _loadgen_has_put_interval_line "$LOADGEN_OUT" \
          && [[ $spins -lt 150 ]]; do
        sleep 0.2
        spins=$((spins + 1))
    done
    if ! _loadgen_has_put_interval_line "$LOADGEN_OUT"; then
        log "WARNING: per-interval PUT rows still absent after ~30s — continuing (analyze may rely on #/AVG line only)"
    fi
    if (( PUT_PHASE_DRAIN_SECS > 0 )); then
        log "Draining ${PUT_PHASE_DRAIN_SECS}s (PUT_PHASE_DRAIN_SECS) before stopping monitors"
        sleep "${PUT_PHASE_DRAIN_SECS}"
    fi
    log "PUT traffic complete — stopping monitors"
    stop_monitors
    LOCAL_MONITOR_PID=""
    declare -A REMOTE_MONITOR_PIDS

    if $FORCE_CLEANUP; then
        log "--force-cleanup: killing loadgen (skip S3 DELETE cleanup phase)"
        kill "$LOADGEN_PID" 2>/dev/null || true
        wait "$LOADGEN_PID" 2>/dev/null || true
        LOADGEN_FORCE_KILLED=true
        log "--- Step 4b: manual bucket cleanup (force-cleanup) ---"
        bash "$LIB/cleanup.sh"
    else
        log "Loadgen DELETE cleanup phase continues..."
    fi
fi

if $LOADGEN_FORCE_KILLED; then
    LOADGEN_STATUS=0
else
    # Includes normal completion (loadgen S3 DELETE cleanup) or error exit
    wait "$LOADGEN_PID"
    LOADGEN_STATUS=$?
fi
log "Loadgen finished (status=${LOADGEN_STATUS})"

# ---------------------------------------------------------------------------
# Step 5 — Stop monitors (no-op if already stopped above; fallback if loadgen exits
#           before #<test>/ AVG summary appears, e.g. on error)
# ---------------------------------------------------------------------------

log "--- Step 5: stop monitors ---"
stop_monitors
# Clear PIDs so the EXIT trap doesn't double-stop
LOCAL_MONITOR_PID=""
declare -A REMOTE_MONITOR_PIDS

# ---------------------------------------------------------------------------
# Step 6 — Collect peer artifacts (also done in EXIT trap, explicit here
#           so analyze.py sees all files before it runs)
# ---------------------------------------------------------------------------

log "--- Step 6: collect peer artifacts ---"
collect_peer_artifacts

# ---------------------------------------------------------------------------
# Step 7 — Collect obs JSONL from node1 (--trace only)
# ---------------------------------------------------------------------------

if $TRACE; then
    log "--- Step 7: collect trace logs ---"
    TRACE_OUT="$OUT/trace"
    mkdir -p "$TRACE_OUT"
    OBS_DIR="${RUSTFS_OBS_LOG_DIR:-/var/logs/rustfs}"
    # Copy the most recent JSONL log (written during this run)
    LATEST_LOG=$(ssh "127.0.0.1" "ls -t '${OBS_DIR}'/*.log 2>/dev/null | head -1" || true)
    if [[ -n "$LATEST_LOG" ]]; then
        scp -q "127.0.0.1:${LATEST_LOG}" "$TRACE_OUT/node1-rustfs.log"
        log "Trace log collected: $TRACE_OUT/node1-rustfs.log"
        # Run timing analysis
        python3 "$SCRIPT_DIR/analyze/obs_timing_from_jsonl.py" \
            --input "$TRACE_OUT/node1-rustfs.log" \
            --csv   "$TRACE_OUT/timing_flat.csv" \
            --html  "$TRACE_OUT/timing_gantt.html" \
            --html-max-traces 60 \
            --html-span-contains get_write_lock \
            || log "WARNING: obs_timing_from_jsonl.py failed — trace files may still be usable"
    else
        log "WARNING: no obs JSONL log found in ${OBS_DIR}"
    fi
fi

# ---------------------------------------------------------------------------
# Step 8 — Analyze
# ---------------------------------------------------------------------------

log "--- Step 8: analyze ---"
if [[ -f "$ANALYZE" ]]; then
    python3 "$ANALYZE" --out "$OUT" --baseline "$BASELINE" \
        && ANALYZE_STATUS=0 || ANALYZE_STATUS=$?
    if [[ -f "$OUT/report.md" ]]; then
        log "--- Report ---"
        cat "$OUT/report.md"
    fi
else
    log "WARNING: analyze.py not found at $ANALYZE — skipping (Phase 2 not yet built)"
    ANALYZE_STATUS=0
fi

# Exit with loadgen status if non-zero, otherwise analyze status
exit $(( LOADGEN_STATUS != 0 ? LOADGEN_STATUS : ANALYZE_STATUS ))
