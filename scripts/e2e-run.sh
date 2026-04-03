#!/usr/bin/env bash
set -ex
# Copyright 2024 RustFS Team
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

BIN=$1
VOLUME=$2

chmod +x "$BIN"
mkdir -p "$VOLUME"

# Avoid shared runners where another RustFS already listens on :9000.
pick_port() {
	if [[ -n "${E2E_RUSTFS_PORT:-}" ]]; then
		echo "$E2E_RUSTFS_PORT"
		return
	fi
	if command -v python3 >/dev/null 2>&1; then
		python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()"
	else
		echo "19080"
	fi
}

PORT="$(pick_port)"
export RUSTFS_E2E_EXTERNAL_ADDR="127.0.0.1:${PORT}"

export RUST_LOG="rustfs=debug,ecstore=debug,s3s=debug,iam=debug"
export RUST_BACKTRACE=full
"$BIN" \
	--address "127.0.0.1:${PORT}" \
	--access-key rustfsadmin \
	--secret-key rustfsadmin \
	"$VOLUME" >/tmp/rustfs.log 2>&1 &
RUSTFS_PID=$!

cleanup_rustfs() {
	kill "${RUSTFS_PID}" 2>/dev/null || true
	wait "${RUSTFS_PID}" 2>/dev/null || true
}
trap cleanup_rustfs EXIT

# Wait for listener (fixed sleep is unreliable on slow hosts)
for _ in $(seq 1 30); do
	if nc -z 127.0.0.1 "${PORT}" 2>/dev/null || timeout 1 bash -c "cat < /dev/null > /dev/tcp/127.0.0.1/${PORT}" 2>/dev/null; then
		break
	fi
	sleep 1
done

export AWS_ACCESS_KEY_ID=rustfsadmin
export AWS_SECRET_ACCESS_KEY=rustfsadmin
export AWS_REGION=us-east-1
export AWS_ENDPOINT_URL="http://127.0.0.1:${PORT}"
export RUST_LOG="s3s_e2e=debug,s3s_test=info,s3s=debug"
export RUST_BACKTRACE=full
s3s-e2e

trap - EXIT
cleanup_rustfs
