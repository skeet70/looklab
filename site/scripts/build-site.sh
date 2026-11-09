#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)

usage() {
  printf 'usage: %s --dist <dir> [--release] [--public-url <prefix>]\n' "$0" >&2
  exit 2
}

dist=
release=false
public_url=/
while [ $# -gt 0 ]; do
  case "$1" in
    --dist)
      [ $# -ge 2 ] || usage
      dist=$2
      shift 2
      ;;
    --dist=*)
      dist=${1#--dist=}
      shift
      ;;
    --release)
      release=true
      shift
      ;;
    --public-url)
      [ $# -ge 2 ] || usage
      public_url=$2
      shift 2
      ;;
    --public-url=*)
      public_url=${1#--public-url=}
      shift
      ;;
    *)
      printf '%s: unknown argument: %s\n' "$0" "$1" >&2
      usage
      ;;
  esac
done
[ -n "$dist" ] || usage

case "$dist" in
  /*) ;;
  *) dist=$PWD/$dist ;;
esac
parent=$(dirname -- "$dist")

case "$public_url" in
  /*) ;;
  *) public_url=/$public_url ;;
esac
case "$public_url" in
  */) ;;
  *) public_url=$public_url/ ;;
esac

target_root=${LOOKLAB_UI_TARGET_DIR:-${CARGO_TARGET_DIR:-$root/target-ui}}
case "$target_root" in
  /*) ;;
  *) target_root=$PWD/$target_root ;;
esac
site_root=$target_root/leptos-site/$$

mkdir -p -- "$parent" "$target_root"
stage=$parent/.$(basename -- "$dist").build.$$
retired=$parent/.$(basename -- "$dist").old.$$
cleanup() {
  if [ -e "$retired" ] && [ ! -e "$dist" ]; then
    mv -- "$retired" "$dist"
  fi
  rm -rf -- "$stage" "$retired" "$site_root"
}
rm -rf -- "$stage" "$retired"
mkdir -p -- "$stage"
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

sh "$root/site/scripts/wasm-assets.sh"

set -- build --split --frontend-only --cargo-locked
if [ "${CARGO_NET_OFFLINE:-}" = true ]; then
  set -- "$@" --cargo-offline
fi
[ "$release" = false ] || set -- "$@" --release
(
  cd "$root/site"
  LEPTOS_SITE_ROOT="$site_root" cargo leptos "$@"
)

pkg=$site_root/pkg
if [ ! -d "$pkg" ]; then
  printf '%s: cargo-leptos left no package directory at %s\n' "$0" "$pkg" >&2
  exit 1
fi
cp -R -- "$pkg" "$stage/pkg"
emitted_css=$stage/pkg/looklab-site.css
if [ -f "$emitted_css" ] && ! grep -q '[^[:space:]]' "$emitted_css"; then
  rm -f -- "$emitted_css"
fi
rm -f -- "$stage"/pkg/*.d.ts

for css in "$root/site/styles.css" "$root"/site/styles/*.css; do
  [ -f "$css" ] || continue
  cp -- "$css" "$stage/$(basename -- "$css")"
done
cp -- "$root/site/favicon.ico" "$stage/favicon.ico"

for name in data workers generated; do
  [ -d "$root/site/$name" ] || continue
  cp -R -- "$root/site/$name" "$stage/$name"
done
cp -- "$root/site/errors.js" "$stage/errors.js"

for file in pkg/looklab-site.wasm pkg/looklab-site.js errors.js \
  workers/prep-worker.js workers/match-worker.js workers/scene-worker.js \
  generated/wasm/looklab_prep_module.wasm generated/wasm/looklab_scene_module.wasm \
  generated/wasm/looklab_match.wasm; do
  if [ ! -f "$stage/$file" ]; then
    printf '%s: missing client asset %s\n' "$0" "$file" >&2
    exit 1
  fi
done

# The release id covers the client assets and the page template that installs diagnostics.
release=$(
  {
    sha256sum -- "$root/site/index.html" | cut -d' ' -f1
    for file in errors.js workers/prep-worker.js workers/match-worker.js \
      workers/scene-worker.js generated/wasm/looklab_prep_module.wasm \
      generated/wasm/looklab_scene_module.wasm generated/wasm/looklab_match.wasm; do
      sha256sum -- "$stage/$file" | cut -d' ' -f1
    done
    find "$stage/pkg" -type f \( -name '*.js' -o -name '*.wasm' \) | LC_ALL=C sort |
      while IFS= read -r file; do
        sha256sum -- "$file" | cut -d' ' -f1
      done
  } | sha256sum | cut -d' ' -f1
)
if [ "${#release}" -ne 64 ]; then
  printf '%s: could not compute a 64-character build id\n' "$0" >&2
  exit 1
fi

api_base=${LOOKLAB_API_BASE:-}
preview=
case "${LOOKLAB_PREVIEW:-}" in
  *[![:space:]]*) preview=1 ;;
esac

escape_replacement() {
  printf '%s' "$1" | sed 's/[\\|&]/\\&/g'
}

sed \
  -e "s|__PREFIX__|$(escape_replacement "$public_url")|g" \
  -e "s|__RELEASE__|$release|g" \
  -e "s|__API_BASE__|$(escape_replacement "$api_base")|g" \
  -e "s|__PREVIEW__|$preview|g" \
  "$root/site/index.html" > "$stage/index.html"

if [ -e "$dist" ]; then
  mv -- "$dist" "$retired"
  if ! mv -- "$stage" "$dist"; then
    mv -- "$retired" "$dist"
    exit 1
  fi
  rm -rf -- "$retired"
else
  mv -- "$stage" "$dist"
fi
