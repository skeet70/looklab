#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# tools/fitbench.sh [args...] - convergence bench for the fit cases that fail today.
# Runs the fitbench example in release through the shared native lane; arguments and
# exit status pass through to it.

set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"

exec nix develop -c tools/build.sh --lane native -- \
  cargo run --release -p looklab-fit --features synth --example fitbench -- "$@"
