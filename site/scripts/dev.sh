#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"
if [[ ${1:-} != --in-shell ]]; then
  exec nix develop "$root" -c bash "$root/site/scripts/dev.sh" --in-shell "$@"
fi
shift
regen_data=false
case "${1:-}" in
  "") ;;
  --regen-data) regen_data=true; shift ;;
  *) printf 'Usage: %s [--regen-data]\n' "$0" >&2; exit 1 ;;
esac
[[ $# = 0 ]] || { printf 'Unexpected arguments\n' >&2; exit 1; }
export CARGO_TARGET_DIR="${LOOKLAB_UI_TARGET_DIR:-$root/target-ui}"
[[ $CARGO_TARGET_DIR = /* ]] || CARGO_TARGET_DIR="$root/$CARGO_TARGET_DIR"
export LOOKLAB_WORKER_WASM="${LOOKLAB_WORKER_WASM:-reuse}"
export CARGO_PROFILE_DEV_DEBUG=0
unset LOOKLAB_PREVIEW LOOKLAB_API_BASE
mkdir -p "$CARGO_TARGET_DIR"
stub_pid=
trunk_pid=
startup_pid=
cleanup() {
  trap - EXIT TERM INT
  [[ -z $trunk_pid ]] || kill -TERM -- "-$trunk_pid" 2>/dev/null || true
  [[ -z $stub_pid ]] || kill -TERM -- "-$stub_pid" 2>/dev/null || true
  [[ -z $startup_pid ]] || kill -TERM -- "-$startup_pid" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT
trap 'exit 0' TERM INT

run_startup() {
  setsid "$@" &
  startup_pid=$!
  wait "$startup_pid"
  startup_pid=
}

if [[ ! -f site/data/scenes/index.json || $regen_data = true ]]; then
  printf 'Generating public site data and camera match bundles\n'
  run_startup cargo run -p looklab-publish --example site_data -j 6 -- site
  printf 'Generating synthetic-match end-to-end fixture\n'
  run_startup cargo run -p looklab-wasm --features fixtures --example publish_match_fixture -j 6 -- site
fi
run_startup cargo build -p looklab-web --example stub_server --features stub

cat > "$CARGO_TARGET_DIR/Trunk.dev.toml" <<EOF
[build]
target = "$root/site/index.html"
dist = "$CARGO_TARGET_DIR/dist"
public_url = "/"
filehash = false

[[hooks]]
stage = "pre_build"
command = "sh"
command_arguments = ["$root/site/scripts/wasm-assets.sh"]

[watch]
watch = ["$root/site", "$root/crates/looklab-web/src"]
ignore = ["$root/site/generated", "$root/site/dist"]

[[proxy]]
backend = "http://127.0.0.1:8098/api/"

[[proxy]]
backend = "http://127.0.0.1:8098/status/"

[[proxy]]
backend = "http://127.0.0.1:8098/log.json"

[[proxy]]
backend = "http://127.0.0.1:8098/lists/"

[[proxy]]
backend = "http://127.0.0.1:8098/bounties.json"

[[proxy]]
backend = "http://127.0.0.1:8098/manifests/"
EOF

setsid "$CARGO_TARGET_DIR/debug/examples/stub_server" \
  --dist "$CARGO_TARGET_DIR/dist" --port 8098 \
  --origin http://localhost:8081 --state "$CARGO_TARGET_DIR/stub-state" &
stub_pid=$!
printf 'looklab UI: http://localhost:8081/ (stub: 8098)\n'
setsid env CARGO_NET_OFFLINE=true trunk serve --config "$CARGO_TARGET_DIR/Trunk.dev.toml" \
  --address 127.0.0.1 --port 8081 --cargo-profile web-dev --release=false &
trunk_pid=$!
wait -n "$stub_pid" "$trunk_pid"
