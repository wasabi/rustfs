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
#   LOADGEN_ENDPOINT  S3 endpoint URL (overrides Server in cfg when set)
#   LOADGEN_HOST      SSH target to run loadgen remotely; empty = run locally

set -euo pipefail

: "${OUT:?Set OUT to the run output directory}"
: "${DURATION:?Set DURATION (e.g. 5m)}"
: "${LOADGEN_BIN:?Set LOADGEN_BIN to the load-generator binary path}"
: "${LOADGEN_CFG:?Set LOADGEN_CFG to the loadgen config file path}"

log() { echo "[loadgen] $(date -u '+%H:%M:%S') $*"; }

# Seed test ID from epoch seconds so each run gets unique bucket names and
# there is no risk of colliding with a previous run's leftover buckets.
TEST_ID="$(date +%s)"

log "Starting load generator (duration=${DURATION} test_id=${TEST_ID})..."
log "Config: ${LOADGEN_CFG}  Endpoint: ${LOADGEN_ENDPOINT:-<from cfg>}"

# ---------------------------------------------------------------------------
# Build the loadgen command
# ---------------------------------------------------------------------------
# Standard two-node EC:1 workload (matches the lab baseline parameters):
#   - Mixed object sizes 0–1500 KB
#   - 20 buckets, 100 concurrent threads
#   - 100% PUT, all other op types forced to 0
#   - AWS-chunked encoding disabled (0%)
#   - PUT disconnect test disabled (0s)
#   - No request timeout cap (0 = none)
#   - deleteOnlyOurBuckets: limits end-of-run S3 DELETE to this run's buckets

LOADGEN_CMD=(
    "$LOADGEN_BIN"
    -c       "$LOADGEN_CFG"
    -z       "0-1500K"
    -b       20
    -t       100
    -put     100
    -get     0
    -del     0
    -list    0
    -head    0
    -post    0
    -awschunked  0
    -disconnect  0
    -errorLimit  0
    -timeout     0
    -dur     "$DURATION"
    -test    "$TEST_ID"
    -deleteOnlyOurBuckets
)

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
    # Expand the array to a quoted string safe for ssh
    ssh "$LOADGEN_HOST" "$(printf '%q ' "${LOADGEN_CMD[@]}")" \
        2>&1 | tee "$LOADGEN_OUT"
else
    log "Running loadgen locally..."
    "${LOADGEN_CMD[@]}" 2>&1 | tee "$LOADGEN_OUT"
fi

# Capture tee-pipeline exit code; bash sets PIPESTATUS after a pipeline
STATUS="${PIPESTATUS[0]}"

if [[ "$STATUS" -eq 0 ]]; then
    log "Load generator finished successfully"
else
    log "Load generator exited with status ${STATUS}"
fi

exit "$STATUS"
