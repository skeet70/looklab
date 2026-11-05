#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate golden vectors for looklab-color's F-Log2 transfer and F-Gamut matrices.

Run from the repo root:

    nix develop .#golden -c python3 tools/golden/gen_flog2.py

Writes ``crates/looklab-color/tests/golden/flog2.json``. Deterministic: fixed
seed, sorted keys, ``repr``-precision floats.
"""

from __future__ import annotations

import numpy as np

from colour.models import RGB_COLOURSPACES
from colour.models.rgb.transfer_functions import log_decoding_FLog2, log_encoding_FLog2

import common

CUT1 = 0.000889
CUT2 = 0.100686685370811


def encode_inputs() -> np.ndarray:
    rng = common.rng(11)
    special = np.array(
        [
            0.0,
            CUT1 - 1e-7,
            CUT1,
            CUT1 + 1e-7,
            0.001,
            0.005,
            0.01,
            0.05,
            0.1,
            0.18,
            0.5,
            1.0,
            2.0,
            5.0,
            10.0,
            20.0,
            50.0,
            60.0,
        ]
    )
    random = rng.uniform(0.0, 60.0, 256)
    return np.concatenate([special, random])


def decode_inputs() -> np.ndarray:
    rng = common.rng(12)
    special = np.array(
        [
            0.0,
            CUT2 - 1e-9,
            CUT2,
            CUT2 + 1e-9,
            0.05,
            0.1,
            0.18,
            0.3,
            0.39100724189123004,
            0.5,
            0.6,
            0.8,
            1.0,
        ]
    )
    random = rng.uniform(0.0, 1.0, 256)
    return np.concatenate([special, random])


def main() -> None:
    encode = encode_inputs()
    decode = decode_inputs()
    space = RGB_COLOURSPACES["F-Gamut"]
    payload = {
        "meta": common.meta("gen_flog2.py"),
        "flog2_encode": common.vectors(
            encode[:, None], log_encoding_FLog2(encode)[:, None]
        ),
        "flog2_decode": common.vectors(
            decode[:, None], log_decoding_FLog2(decode)[:, None]
        ),
        "fgamut": {
            "primaries": [
                [0.708, 0.292],
                [0.170, 0.797],
                [0.131, 0.046],
            ],
            "white_xy": [0.3127, 0.329],
            "rgb_to_xyz": common.floats(space.matrix_RGB_to_XYZ),
            "xyz_to_rgb": common.floats(space.matrix_XYZ_to_RGB),
        },
    }
    common.write_json(common.ROOT / "crates/looklab-color/tests/golden/flog2.json", payload)


if __name__ == "__main__":
    main()
