#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate golden vectors for looklab-color's DNG temperature conversions.

colour-science 0.4.7 exposes Robertson (1968) through the CIE 1960 UCS
functions rather than through ``xy_to_CCT(method="Robertson 1968")``; the
conversion used here is ``XY_to_uv`` followed by
``uv_to_CCT_Robertson1968``, and its inverse ``CCT_to_uv_Robertson1968``
followed by ``uv_to_XY``.
"""

from __future__ import annotations

import numpy as np

from colour.models import UCS_uv_to_xy, xy_to_UCS_uv
from colour.temperature.robertson1968 import (
    CCT_to_uv_Robertson1968,
    uv_to_CCT_Robertson1968,
)

import common


def main() -> None:
    rng = common.rng(7)

    xy_inputs = np.stack(
        [rng.uniform(0.24, 0.46, 200), rng.uniform(0.22, 0.44, 200)], axis=-1
    )
    xy_outputs = [
        [uv_to_CCT_Robertson1968(xy_to_UCS_uv(row))[0]] for row in xy_inputs
    ]

    cct_inputs = np.exp(rng.uniform(np.log(1_800.0), np.log(25_000.0), 200))
    cct_outputs = [
        list(UCS_uv_to_xy(CCT_to_uv_Robertson1968([cct, 0.0]))) for cct in cct_inputs
    ]

    payload = {
        "meta": common.meta("gen_dng.py"),
        "xy_to_cct": common.vectors(xy_inputs, xy_outputs),
        "cct_to_xy": common.vectors(
            [[float(cct)] for cct in cct_inputs], cct_outputs
        ),
    }
    common.write_json(
        common.ROOT / "crates/looklab-color/tests/golden/dng.json", payload
    )


if __name__ == "__main__":
    main()
