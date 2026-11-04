#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# Download and verify the RAW fixtures listed in fixtures/manifest.toml.
# `--check` verifies without downloading.
# Committed fixtures land in fixtures/, fetch-only ones in fixtures/cache/.
# curl and sha256sum come from PATH, or from nix when absent.

set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
manifest="$root/fixtures/manifest.toml"
check_only=0
if [[ ${1:-} == "--check" ]]; then
  check_only=1
elif [[ $# -gt 0 ]]; then
  echo "usage: $0 [--check]" >&2
  exit 2
fi

fetch() {
  local dest=$1 url=$2
  local tmp="$dest.part"
  mkdir -p "$(dirname "$dest")"
  rm -f "$tmp"
  if command -v curl >/dev/null 2>&1; then
    curl --silent --show-error --fail --location --retry 3 --output "$tmp" "$url"
  else
    nix shell nixpkgs#curl -c curl --silent --show-error --fail --location --retry 3 --output "$tmp" "$url"
  fi
  mv "$tmp" "$dest"
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    nix shell nixpkgs#coreutils -c sha256sum "$1" | cut -d' ' -f1
  fi
}

read_manifest() {
  local reader
  reader=$(cat <<'PY'
import sys, tomllib

with open(sys.argv[1], "rb") as fh:
    manifest = tomllib.load(fh)
for fixture in manifest["fixture"]:
    committed = "true" if fixture["committed"] else "false"
    print("\t".join([fixture["file"], fixture["source_url"], fixture["sha256"], str(fixture["size"]), committed]))
PY
)
  if command -v python3 >/dev/null 2>&1; then
    python3 -c "$reader" "$manifest"
  else
    nix shell nixpkgs#python3 -c python3 -c "$reader" "$manifest"
  fi
}

status=0
while IFS=$'\t' read -r file url sha size committed; do
  if [[ $committed == "true" ]]; then
    dest="$root/fixtures/$file"
  else
    dest="$root/fixtures/cache/$file"
  fi

  if [[ -f $dest ]]; then
    actual_size=$(wc -c <"$dest")
    actual_sha=$(sha256 "$dest")
    if [[ $actual_size == "$size" && $actual_sha == "$sha" ]]; then
      echo "ok       $file"
      continue
    fi
    echo "corrupt  $file" >&2
    status=1
    if [[ $check_only == 1 ]]; then
      continue
    fi
  elif [[ $check_only == 1 ]]; then
    echo "missing  $file" >&2
    status=1
    continue
  fi

  echo "fetch    $file"
  fetch "$dest" "$url"
  actual_size=$(wc -c <"$dest")
  actual_sha=$(sha256 "$dest")
  if [[ $actual_size != "$size" || $actual_sha != "$sha" ]]; then
    echo "checksum mismatch for $file: got $actual_size bytes, sha256 $actual_sha" >&2
    rm -f "$dest"
    status=1
  fi
done < <(read_manifest)

exit "$status"
