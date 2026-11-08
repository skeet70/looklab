#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Production-shaped local launch on one origin.
set -euo pipefail
umask 077

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

data_dir=${LOOKLAB_DATA_DIR:-$root/data}
state=${LOOKLAB_STATE_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/looklab}
port=${LOOKLAB_PORT:-8080}
origin="http://localhost:$port"
dist="$state/site/dist"
data="$state/site/data"
server="$root/target/debug/looklab-server"
worker="$root/target/debug/looklab-worker"

mkdir -p "$state/blobs" "$state/community" "$data"

nix develop "$root" -c "$root/tools/build.sh" --lane native -- \
  cargo build --locked -p looklab-server -p looklab-worker
(
  unset LOOKLAB_PREVIEW LOOKLAB_API_BASE
  nix develop "$root" -c "$root/tools/build.sh" --lane ui -- \
    sh site/scripts/build-site.sh --dist "$dist"
)

export LOOKLAB_BIND="127.0.0.1:$port"
export LOOKLAB_DB_PATH="$state/looklab.sqlite3"
export LOOKLAB_BLOB_DIR="$state/blobs"
export LOOKLAB_RP_ID=localhost
export LOOKLAB_ORIGIN="$origin"
export LOOKLAB_SERVER_SECRET_PATH="$state/server-secret"
export LOOKLAB_BODIES_DIR="$data_dir/bodies"
export LOOKLAB_SNAPSHOT_DIR="$state/community"
export LOOKLAB_SITE_DIST="$dist"
export LOOKLAB_SITE_DATA="$data"

"$server" init

"$worker" \
  --db "$LOOKLAB_DB_PATH" \
  --blobs "$LOOKLAB_BLOB_DIR" \
  --data "$data_dir" \
  --snapshot-dir "$LOOKLAB_SNAPSHOT_DIR" \
  --site "$state/site" \
  --transforms "$data_dir/transforms" \
  --lease-seconds 300 &
worker_pid=$!
server_pid=

cleanup() {
  local status=${1:-0}
  trap - EXIT INT TERM
  local pid
  for pid in "$server_pid" "$worker_pid"; do
    if [[ -n $pid ]]; then kill -TERM "$pid" 2>/dev/null || true; fi
  done
  local attempt alive
  for attempt in $(seq 1 60); do
    alive=0
    for pid in "$server_pid" "$worker_pid"; do
      if [[ -n $pid ]] && kill -0 "$pid" 2>/dev/null; then alive=1; fi
    done
    if (( alive == 0 )); then break; fi
    sleep 1
  done
  for pid in "$server_pid" "$worker_pid"; do
    if [[ -n $pid ]]; then
      kill -KILL "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  exit "$status"
}

trap 'cleanup $?' EXIT
trap 'cleanup 130' INT
trap 'cleanup 143' TERM

for _ in $(seq 1 120); do
  [[ -f $data/index.json ]] && break
  sleep 1
done
if [[ ! -f $data/index.json ]]; then
  printf 'warning: no published index yet; /readyz stays 503 until the worker publishes\n' >&2
fi

printf 'looklab on %s (state %s)\n' "$origin" "$state" >&2
printf 'first operator: register a passkey, then run:\n' >&2
printf '  LOOKLAB_DB_PATH=%s %s bootstrap-admin --display-handle <name> --reason "first admin bootstrap"\n' \
  "$LOOKLAB_DB_PATH" "$server" >&2
"$server" &
server_pid=$!

set +e
wait -n "$server_pid" "$worker_pid"
status=$?
set -e
cleanup "$status"
