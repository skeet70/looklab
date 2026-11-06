#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-or-later
set -eu

root=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)
exec python3 "$root/tools/build_products.py" wasm "$@"
