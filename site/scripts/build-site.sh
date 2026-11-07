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

for name in data workers generated; do
  [ -d "$root/site/$name" ] || continue
  cp -R -- "$root/site/$name" "$stage/$name"
done

sed "s|__PREFIX__|$public_url|g" "$root/site/index.html" > "$stage/index.html"

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
