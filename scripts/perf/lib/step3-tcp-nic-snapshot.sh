#!/usr/bin/env bash
# T0/T1 snapshot -- ss summary, nstat -az, optional ss -ti to peer, optional ethtool -S per NIC.
# Called by monitor.sh at test start (T0) and end (T1) to capture TCP and NIC counters.
#
# Usage:
#   bash step3-tcp-nic-snapshot.sh T0 /path/to/out 1 node2:9000 eno1409 eno1419
#   bash step3-tcp-nic-snapshot.sh T1 /path/to/out 2              # no peer, no ethtool
#
# Args:
#   $1 = T0 or T1
#   $2 = output directory
#   $3 = NODE_ID (integer node number used in output filenames)
#   $4 = optional PEER_HOST:PORT -- if set, captures ss -ti to that peer
#   $5 ... = optional NIC interface names for ethtool -S (e.g. eno1409 eno1419)
set -euo pipefail

TAG="${1:?T0 or T1}"
OUT="${2:?output directory}"
NID="${3:?node id}"
PEER_SPEC="${4-}"
NICS=("${@:5}")

mkdir -p "$OUT"

stamp() { date -u '+%Y-%m-%dT%H:%M:%SZ'; }

SSS="$OUT/ss-s-node${NID}-${TAG}.txt"
NST="$OUT/nstat-node${NID}-${TAG}.txt"

{
  stamp
  echo "=== ss -s ==="
  ss -s || true
} > "$SSS"

{
  stamp
  echo "=== nstat -az ==="
  nstat -az || true
} > "$NST"

if [[ -n "$PEER_SPEC" ]]; then
  PEER="${PEER_SPEC%%:*}"
  PORT="${PEER_SPEC##*:}"
  if [[ "$PEER" == "$PEER_SPEC" ]]; then
    echo "Peer arg must be HOST:PORT (got '$PEER_SPEC')" >&2
    exit 1
  fi
  SPEER="$OUT/ss-peer-node${NID}-${TAG}.txt"
  {
    stamp
    echo "=== ss -Hti dst $PEER dport = :$PORT ==="
    ss -Hti "dst $PEER" "dport = :$PORT" 2>/dev/null || ss -Hti "dst $PEER" 2>/dev/null || true
  } > "$SPEER"
fi

if [[ ${#NICS[@]} -ge 1 ]]; then
  ETH="$OUT/ethtool-node${NID}-${TAG}.txt"
  {
    stamp
    for iface in "${NICS[@]}"; do
      echo "=== ethtool -S $iface (filtered) ==="
      ethtool -S "$iface" 2>/dev/null | grep -Ei 'drop|err|discard|miss|fail' || true
    done
  } > "$ETH"
fi

echo "Wrote snapshots under $OUT (${TAG})"
