#!/usr/bin/env bash
# Run one shell command on each self-hosted runner host via SSH.
#
# Default hosts match the rustfs fleet; override with RUNNER_HOSTS (space-separated).
# Optional: RUNNER_SSH_USER (e.g. ubuntu) — if unset, SSH uses your config/default user.
#
# Examples:
#   ./scripts/run-on-runners.sh 'uname -a'
#   ./scripts/run-on-runners.sh -j 2 'df -h /'    # at most 2 SSH sessions at once
#   ./scripts/run-on-runners.sh -j 1 'uptime'      # sequential (no parallelism)
#   RUNNER_SSH_USER=ubuntu ./scripts/run-on-runners.sh 'sudo systemctl status actions.runner.* --no-pager'

set -euo pipefail

RUNNER_HOSTS_DEFAULT="r202-u22 r202-u25 r202-u26 r202-u28 r202-u29"
RUNNER_HOSTS="${RUNNER_HOSTS:-$RUNNER_HOSTS_DEFAULT}"

PARALLEL_JOBS=0

usage() {
	echo "usage: $0 [-j N] <command>" >&2
	echo "  -j N  max concurrent SSH sessions (default: 0 = all hosts at once). Use -j 1 for sequential." >&2
	echo "  <command> is passed to bash -lc on each host (quote if it contains spaces or metacharacters)." >&2
}

while getopts :j:h opt; do
	case $opt in
	j) PARALLEL_JOBS=$OPTARG ;;
	h)
		usage
		exit 0
		;;
	*)
		usage
		exit 1
		;;
	esac
done
shift $((OPTIND - 1))

if [[ $# -lt 1 ]]; then
	usage
	exit 1
fi

if ! [[ "$PARALLEL_JOBS" =~ ^[0-9]+$ ]]; then
	echo "error: -j must be a non-negative integer" >&2
	exit 1
fi

remote_cmd=$*

ssh_target() {
	local host=$1
	if [[ -n "${RUNNER_SSH_USER:-}" ]]; then
		printf '%s@%s' "$RUNNER_SSH_USER" "$host"
	else
		printf '%s' "$host"
	fi
}

safe_name() {
	# Log/rc filenames per host (avoid / : in paths).
	tr '/:' '__' <<<"$1"
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

running=0
for host in $RUNNER_HOSTS; do
	base=$(safe_name "$host")
	log="$tmp/$base.log"
	rcfile="$tmp/$base.rc"
	if ((PARALLEL_JOBS > 0)); then
		while ((running >= PARALLEL_JOBS)); do
			wait -n || true
			((running--)) || true
		done
	fi
	(
		ssh -o BatchMode=yes -o ConnectTimeout=15 "$(ssh_target "$host")" bash -lc "$(printf '%q' "$remote_cmd")" >"$log" 2>&1
		echo $? >"$rcfile"
	) &
	((running++)) || true
done
wait

status=0
for host in $RUNNER_HOSTS; do
	base=$(safe_name "$host")
	echo "=== $(ssh_target "$host") ==="
	cat "$tmp/$base.log"
	read -r r <"$rcfile" || r=1
	if [[ "$r" != 0 ]]; then
		status=1
	fi
	echo
done

exit "$status"
