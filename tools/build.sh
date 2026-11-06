#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
#
# tools/build.sh [--lane ui] [--] [command...]; no command runs the native prepare+test lane.

set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
exec python3 "$root/tools/source_generations.py" run "$@"
