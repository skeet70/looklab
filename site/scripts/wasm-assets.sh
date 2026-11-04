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
# Trunk runs hooks from site/, so a relative CARGO_TARGET_DIR would put artifacts under site/target.
export CARGO_TARGET_DIR="$target"

cargo build --manifest-path "$root/Cargo.toml" -p looklab-prep-module \
  --target wasm32-unknown-unknown --release
cargo build --manifest-path "$root/Cargo.toml" -p looklab-wasm \
  --no-default-features --features worker --target wasm32-unknown-unknown --release

cp "$target/wasm32-unknown-unknown/release/looklab_prep_module.wasm" \
  "$dist/looklab_prep_module.wasm"
cp "$target/wasm32-unknown-unknown/release/looklab_wasm.wasm" \
  "$dist/looklab_match.wasm"

for wasm in "$dist/looklab_prep_module.wasm" "$dist/looklab_match.wasm"; do
  # `--remove-imports` stubs the wasm-bindgen runtime imports the dependency edge emits, so the
  # workers instantiate with `{}`. Seeds arrive in each request; nothing draws randomness in-module.
  wasm-opt --remove-imports -O3 "$wasm" -o "$wasm.opt"
  mv "$wasm.opt" "$wasm"
  wasm-tools validate --features=-relaxed-simd "$wasm"
  if wasm-objdump -x "$wasm" | grep -q '^Import\['; then
    echo "$wasm still imports after --remove-imports" >&2
    exit 1
  fi
done
