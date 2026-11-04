#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Export Pointer's gamut for the v1 probe set.

Source data: colour-science 0.4.7 ``DATA_POINTER_GAMUT_VOLUME`` and
``CCS_POINTER_GAMUT_BOUNDARY``. The volume is ``(L*, C*ab, h_ab)`` on a
15..90 step-5 lightness grid and a 0..350 step-10 hue grid, tabulated under
Pointer's illuminant C; the derived boundary is chromatically adapted to D65
with Bradford and reported as xy.

Run from the repo root:

    nix develop .#golden -c python3 tools/golden/gen_pointer.py
"""

from __future__ import annotations

import sys
from pathlib import Path

import colour
import numpy as np
from colour.models import (
    CCS_ILLUMINANT_POINTER_GAMUT,
    CCS_POINTER_GAMUT_BOUNDARY,
    DATA_POINTER_GAMUT_VOLUME,
)

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

LIGHTNESS = 49.5
SAMPLE_LIGHTNESSES = (45, 50)
HUE_STEP = 10


def max_chroma(lightness: float, hue: float) -> float:
    lower, upper = SAMPLE_LIGHTNESSES
    column = int(hue // HUE_STEP)

    def chroma_at(level: int) -> float:
        row = (level - 15) // 5 * 36 + column
        return float(DATA_POINTER_GAMUT_VOLUME[row, 1])

    fraction = (lightness - lower) / (upper - lower)
    return chroma_at(lower) + fraction * (chroma_at(upper) - chroma_at(lower))


def d65_boundary() -> list[dict]:
    white_c = colour.xy_to_XYZ(tuple(CCS_ILLUMINANT_POINTER_GAMUT))
    white_d65 = colour.xy_to_XYZ((0.3127, 0.3290))
    points = []
    for hue in range(0, 360, HUE_STEP):
        chroma = max_chroma(LIGHTNESS, hue)
        lab = colour.LCHab_to_Lab(np.array([LIGHTNESS, chroma, hue]))
        xyz_c = colour.Lab_to_XYZ(lab, illuminant=CCS_ILLUMINANT_POINTER_GAMUT)
        xyz_d65 = colour.chromatic_adaptation(xyz_c, white_c, white_d65, transform="Bradford")
        x, y = colour.XYZ_to_xy(xyz_d65)
        points.append({"hue_deg": float(hue), "chroma_lab": chroma, "xy": [float(x), float(y)]})
    return points


def main() -> None:
    payload = {
        "source": (
            "colour-science 0.4.7 DATA_POINTER_GAMUT_VOLUME and "
            "CCS_POINTER_GAMUT_BOUNDARY; M. R. Pointer (1980), "
            "The gamut of real surface colours, Color Research & Application 5(3)"
        ),
        "colour_science_version": colour.__version__,
        "numpy_version": np.__version__,
        "lightness_star": LIGHTNESS,
        "hue_step_degrees": HUE_STEP,
        "derived_illuminant": "D65 (Bradford adapted from Pointer's illuminant C)",
        "pointer_illuminant_xy": [float(value) for value in CCS_ILLUMINANT_POINTER_GAMUT],
        "pointer_boundary_xy": [
            [float(x), float(y)] for x, y in CCS_POINTER_GAMUT_BOUNDARY
        ],
        "pointer_volume_lchab": [
            [float(value) for value in row] for row in DATA_POINTER_GAMUT_VOLUME
        ],
        "boundary_d65": d65_boundary(),
    }
    common.write_json(common.ROOT / "data/probes/pointer_gamut.json", payload)


if __name__ == "__main__":
    main()
