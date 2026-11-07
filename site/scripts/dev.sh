#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
cd "$root"
if [[ ${1:-} != --in-shell ]]; then
  exec nix develop "$root" -c bash "$root/site/scripts/dev.sh" --in-shell "$@"
fi
shift
regen_data=false
reuse_data=false
once=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    --regen-data) regen_data=true ;;
    --reuse-data) reuse_data=true ;;
    --once) once=true ;;
    *) printf 'Usage: %s [--once] [--regen-data|--reuse-data]\n' "$0" >&2; exit 1 ;;
  esac
  shift
done

export CARGO_TARGET_DIR="${LOOKLAB_UI_TARGET_DIR:-$root/target-ui}"
[[ $CARGO_TARGET_DIR = /* ]] || CARGO_TARGET_DIR="$root/$CARGO_TARGET_DIR"
export CARGO_PROFILE_DEV_DEBUG=0
unset LOOKLAB_PREVIEW LOOKLAB_API_BASE
mkdir -p "$CARGO_TARGET_DIR"
target_root=$(CDPATH='' cd -- "$CARGO_TARGET_DIR" && pwd -P)

exec 9>"$target_root/dev.lock"
if ! flock -n 9; then
  printf 'Another dev.sh owns %s; stop it first\n' "$target_root" >&2
  exit 1
fi

export LOOKLAB_SOURCE_ORIGIN="$root"
export LOOKLAB_UI_TARGET_DIR="$target_root"
export CARGO_TARGET_DIR="$target_root"

args=()
[[ $regen_data = false ]] || args+=(--regen-data)
[[ $reuse_data = false ]] || args+=(--reuse-data)
[[ $once = false ]] || args+=(--once)
exec python3 "$root/tools/dev_loop.py" "${args[@]}"
