#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
dist="$root/site/generated/wasm"
build_workers=true
build_scene=true
case "${LOOKLAB_WORKER_WASM:-build}" in
  build) ;;
  reuse)
    if [ -s "$dist/looklab_prep_module.wasm" ] && [ -s "$dist/looklab_match.wasm" ]; then
      echo "Reusing prep and match wasm (LOOKLAB_WORKER_WASM=reuse)"
      build_workers=false
    else
      echo "Worker wasm missing; building release workers once"
    fi
    if [ -s "$dist/looklab_scene_module.wasm" ]; then
      build_scene=false
    fi
    ;;
  *)
    echo "LOOKLAB_WORKER_WASM must be build or reuse" >&2
    exit 1
    ;;
esac
case "${LOOKLAB_SCENE_WASM:-auto}" in
  auto) ;;
  build) build_scene=true ;;
  only)
    build_scene=true
    build_workers=false
    ;;
  *)
    echo "LOOKLAB_SCENE_WASM must be auto, build or only" >&2
    exit 1
    ;;
esac
mkdir -p "$dist"

target="${CARGO_TARGET_DIR:-target}"
case "$target" in
  /*) ;;
  *) target="$root/$target" ;;
esac
# Trunk runs hooks from site/, so a relative CARGO_TARGET_DIR would put artifacts under site/target.
export CARGO_TARGET_DIR="$target"

wasm_assets=
if [ "$build_workers" = true ]; then
  cargo build --manifest-path "$root/Cargo.toml" -p looklab-prep-module \
    --target wasm32-unknown-unknown --release
  cargo build --manifest-path "$root/Cargo.toml" -p looklab-wasm \
    --no-default-features --features worker --target wasm32-unknown-unknown --release

  cp "$target/wasm32-unknown-unknown/release/looklab_prep_module.wasm" \
    "$dist/looklab_prep_module.wasm"
  cp "$target/wasm32-unknown-unknown/release/looklab_wasm.wasm" \
    "$dist/looklab_match.wasm"
  wasm_assets="looklab_prep_module.wasm looklab_match.wasm"
fi
if [ "$build_scene" = true ]; then
  cargo build --manifest-path "$root/Cargo.toml" -p looklab-scene-module \
    --target wasm32-unknown-unknown --release
  cp "$target/wasm32-unknown-unknown/release/looklab_scene_module.wasm" \
    "$dist/looklab_scene_module.wasm"
  wasm_assets="$wasm_assets looklab_scene_module.wasm"
fi

for asset in $wasm_assets; do
  wasm="$dist/$asset"
  # Worker modules instantiate with `{}`; dependency edges emit wasm-bindgen runtime imports.
  wasm-opt --remove-imports -O3 "$wasm" -o "$wasm.opt"
  mv "$wasm.opt" "$wasm"
  wasm-tools validate --features=-relaxed-simd "$wasm"
  if wasm-objdump -x "$wasm" | grep -q '^Import\['; then
    echo "$wasm still imports after --remove-imports" >&2
    exit 1
  fi
done
