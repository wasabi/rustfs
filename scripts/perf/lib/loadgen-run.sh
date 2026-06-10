#!/usr/bin/env bash
# lib/loadgen-run.sh — run the wasabi load generator with the standard perf flags.
#
# Blocks until the loadgen exits, tees stdout+stderr to $OUT/loadgen.txt,
# and exits with the loadgen's own exit code.
#
# Called by run-perf-test.sh after deploy and monitors are up.
#
# Required env (set by run-perf-test.sh):
#   OUT       output directory; loadgen.txt is written here
#   DURATION  run duration passed through to -dur (e.g. "5m" or "10m")
#
# Required env (sourced from conf/paths.env by the caller):
#   LOADGEN_BIN       path to the load-generator binary
#   LOADGEN_CFG       path to the loadgen JSON config file (Server, Access, Secret, Bucket)
#   LOADGEN_HOST      SSH target to run loadgen remotely; empty = run locally
#
# Optional (documentation only — not read by this script):
#   LOADGEN_ENDPOINT  May appear in paths.env for operator notes; S3 URL must be set as Server in LOADGEN_CFG.

set -euo pipefail

: "${OUT:?Set OUT to the run output directory}"
: "${DURATION:?Set DURATION (e.g. 5m)}"
: "${LOADGEN_BIN:?Set LOADGEN_BIN to the load-generator binary path}"
: "${LOADGEN_CFG:?Set LOADGEN_CFG to the loadgen config file path}"

log() { echo "[loadgen] $(date -u '+%H:%M:%S') $*"; }

# Seed test ID from epoch seconds so each run gets unique bucket names and
# there is no risk of colliding with a previous run's leftover buckets.
TEST_ID="$(date +%s)"
# Allow callers to pin the test ID (e.g. to resume a prior preload pool).
[[ -n "${LG_TEST_ID:-}" ]] && TEST_ID="$LG_TEST_ID"

log "Starting load generator (duration=${DURATION} test_id=${TEST_ID})..."
log "Config: ${LOADGEN_CFG}  Endpoint: ${LOADGEN_ENDPOINT:-<from cfg>}"

# ---------------------------------------------------------------------------
# Workload parameters — override via env vars; defaults give the standard
# PUT-only baseline identical to all prior A/B runs.
#
#   LG_PUT_PCT      PUT percentage  (default: 100)
#   LG_GET_PCT      GET percentage  (default: 0)
#   LG_DEL_PCT      DELETE percentage (default: 0)
#   LG_LIST_PCT     LIST percentage (default: 0)
#   LG_HEAD_PCT     HEAD percentage (default: 0)
#   LG_OBJ_SIZE     object size range passed to -z (default: 0-1500K)
#   LG_THREADS      worker thread count passed to -t (default: 100)
#   LG_PUTS_PRELOAD integer: pre-populate N objects before the timed phase;
#                   passed to -puts only when > 0; useful for GET/DELETE runs
#                   where the pool must exist before the op starts
#   LG_TEST_ID      override the auto-generated test ID; use to resume a prior
#                   preload pool (always pair with LG_RESUME=1 and
#                   LG_NO_DELETE_BUCKETS_BEFORE=1 to avoid silent pool wipe)
#   LG_SAVE         pass -save to loadgen (saves object map for later resume)
#   LG_RESUME       pass -resume to loadgen (restores saved object map);
#                   MUST be paired with LG_NO_DELETE_BUCKETS_BEFORE=1
#   LG_RESUME_SKIP_CHECK  pass -resumeSkipCheck to loadgen (skip CheckSavedObjects /
#                   ListObjectsV2 scan on both save and resume; required for large
#                   pools where ListObjectsV2 times out; also avoids cache re-warm)
#   LG_NO_DELETE_BUCKETS_BEFORE  pass -noDeleteBucketsBefore (skip pre-run wipe)
#   LG_NO_DELETE_BUCKETS_AFTER   pass -noDeleteBucketsAfter (skip post-run wipe)
# ---------------------------------------------------------------------------

LG_PUT_PCT="${LG_PUT_PCT:-100}"
LG_GET_PCT="${LG_GET_PCT:-0}"
LG_DEL_PCT="${LG_DEL_PCT:-0}"
LG_LIST_PCT="${LG_LIST_PCT:-0}"
LG_HEAD_PCT="${LG_HEAD_PCT:-0}"
LG_OBJ_SIZE="${LG_OBJ_SIZE:-0-1500K}"
LG_THREADS="${LG_THREADS:-100}"
LG_PUTS_PRELOAD="${LG_PUTS_PRELOAD:-0}"
LG_SAVE="${LG_SAVE:-0}"
LG_RESUME="${LG_RESUME:-0}"
LG_RESUME_SKIP_CHECK="${LG_RESUME_SKIP_CHECK:-0}"
LG_NO_DELETE_BUCKETS_BEFORE="${LG_NO_DELETE_BUCKETS_BEFORE:-0}"
LG_NO_DELETE_BUCKETS_AFTER="${LG_NO_DELETE_BUCKETS_AFTER:-0}"

# ---------------------------------------------------------------------------
# Build the loadgen command
# ---------------------------------------------------------------------------

# Line-buffer through the pipe to tee. stdout to a pipe is often fully buffered;
# without stdbuf, summary lines can hit loadgen.txt before prior interval rows,
# leaving analyze.py with zero parsed intervals after an early SIGTERM.
# Also line-buffer tee's writes to loadgen.txt (disk files are fully buffered by default).

STDBUF=()
if command -v stdbuf >/dev/null 2>&1; then
    STDBUF=(stdbuf -oL -eL)
fi

# Second stdbuf instance for tee (must be a separate array expansion in bash).
TEE_IO=()
if [[ ${#STDBUF[@]} -gt 0 ]]; then
    TEE_IO=("${STDBUF[@]}")
fi

LOADGEN_CMD=(
    "${STDBUF[@]}"
    "$LOADGEN_BIN"
    -c       "$LOADGEN_CFG"
    -z       "$LG_OBJ_SIZE"
    -b       20
    -t       "$LG_THREADS"
    -put     "$LG_PUT_PCT"
    -get     "$LG_GET_PCT"
    -del     "$LG_DEL_PCT"
    -list    "$LG_LIST_PCT"
    -head    "$LG_HEAD_PCT"
    -post    0
    -awschunked  0
    -disconnect  0
    -errorLimit  0
    -timeout     0
    -dur     "$DURATION"
    -test    "$TEST_ID"
    -v4
    -deleteOnlyOurBuckets
)

if (( LG_PUTS_PRELOAD > 0 )); then
    LOADGEN_CMD+=( -puts "$LG_PUTS_PRELOAD" )
fi

[[ "$LG_SAVE"                     == "1" ]] && LOADGEN_CMD+=( -save )
[[ "$LG_RESUME"                   == "1" ]] && LOADGEN_CMD+=( -resume )
[[ "$LG_RESUME_SKIP_CHECK"        == "1" ]] && LOADGEN_CMD+=( -resumeSkipCheck )
[[ "$LG_NO_DELETE_BUCKETS_BEFORE" == "1" ]] && LOADGEN_CMD+=( -noDeleteBucketsBefore )
[[ "$LG_NO_DELETE_BUCKETS_AFTER"  == "1" ]] && LOADGEN_CMD+=( -noDeleteBucketsAfter )

# Server endpoint comes from the Server field in LOADGEN_CFG (local.cfg).
# The binary does not accept a command-line server override flag.

mkdir -p "$OUT"
LOADGEN_OUT="$OUT/loadgen.txt"

# Write the test ID to a sidecar file so cleanup.sh and the analyzer can
# reference it without parsing loadgen.txt.
echo "$TEST_ID" > "$OUT/loadgen-test-id.txt"

log "Output → ${LOADGEN_OUT}"
log "Test ID → ${TEST_ID} (saved to loadgen-test-id.txt)"

# ---------------------------------------------------------------------------
# Execute — locally or via SSH
# ---------------------------------------------------------------------------

if [[ -n "${LOADGEN_HOST:-}" ]]; then
    log "Running loadgen remotely on ${LOADGEN_HOST}..."
    # Expand the array to a quoted string safe for ssh (same stdbuf recipe as locally)
    ssh "$LOADGEN_HOST" "$(printf '%q ' "${LOADGEN_CMD[@]}")" \
        2>&1 | "${TEE_IO[@]}" tee "$LOADGEN_OUT"
else
    log "Running loadgen locally..."
    "${LOADGEN_CMD[@]}" 2>&1 | "${TEE_IO[@]}" tee "$LOADGEN_OUT"
fi

# Capture tee-pipeline exit code; bash sets PIPESTATUS after a pipeline
STATUS="${PIPESTATUS[0]}"

if [[ "$STATUS" -eq 0 ]]; then
    log "Load generator finished successfully"
else
    log "Load generator exited with status ${STATUS}"
fi

exit "$STATUS"
