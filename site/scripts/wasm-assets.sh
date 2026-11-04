#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
dist="$root/site/generated/wasm"
mkdir -p "$dist"

target="${CARGO_TARGET_DIR:-target}"
case "$target" in
  /*) ;;
  *) target="$root/$target" ;;
esac

cargo build --manifest-path "$root/Cargo.toml" -p looklab-prep-module \
  --target wasm32-unknown-unknown --release
cargo build --manifest-path "$root/Cargo.toml" -p looklab-wasm \
  --no-default-features --features worker --target wasm32-unknown-unknown --release

cp "$target/wasm32-unknown-unknown/release/looklab_prep_module.wasm" \
  "$dist/looklab_prep_module.wasm"
cp "$target/wasm32-unknown-unknown/release/looklab_wasm.wasm" \
  "$dist/looklab_match.wasm"

for wasm in "$dist/looklab_prep_module.wasm" "$dist/looklab_match.wasm"; do
  wasm-opt --remove-imports -O3 "$wasm" -o "$wasm.opt"
  mv "$wasm.opt" "$wasm"
  wasm-tools validate --features=-relaxed-simd "$wasm"
done
