#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate the CCT reference for the committed RAW fixtures.

Writes ``crates/looklab-body/tests/golden/fixtures_cct.json``. The reference
reads the file's as-shot white-balance coefficients and colour matrices with
LibRaw's ``raw-identify -v`` and solves the DNG neutral (DNG 1.6 chapter 6,
"NeutralToXY") with numpy and colour-science's Robertson (1968) temperature
conversion, so the test checks ``looklab-color::dng`` against an independent
implementation of the same specification (docs/09 P4.1, 150 K tolerance).

Requires ``raw-identify`` (nixpkgs ``libraw``), provided by ``devShells.golden``.
"""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

import numpy as np
from colour.models import UCS_uv_to_xy, xy_to_UCS_uv
from colour.temperature.robertson1968 import uv_to_CCT_Robertson1968

import common

FIXTURES = ("canon-eos-60d.dng", "fujifilm-x-s10.raf")

# DNG calibration illuminants: temperature for 1/T interpolation and the
# standard illuminant chromaticity used for adaptation.
ILLUMINANTS = {
    "Illuminant A": (2856.0, (0.44758, 0.40745)),
    "D50": (5000.0, (0.3457, 0.3585)),
    "D55": (5500.0, (0.33243, 0.34744)),
    "D65": (6504.0, (0.3127, 0.3290)),
    "D75": (7500.0, (0.29903, 0.31488)),
}


def identify(path: Path) -> str:
    return subprocess.run(
        ["raw-identify", "-v", str(path)],
        check=True,
        capture_output=True,
        text=True,
    ).stdout


def as_shot_coefficients(text: str) -> list[float]:
    """``[R, G, B]`` from the makernote as-shot row ``R G1 B 0``."""
    for line in text.splitlines():
        if line.strip().startswith("As shot"):
            return [float(value) for value in line.split()[2:5]]
    raise ValueError("no as-shot white balance in raw-identify output")


def matrices(text: str) -> list[tuple[str, list[list[float]]]]:
    lines = text.splitlines()
    illuminants = {
        int(match.group(1)): match.group(2).strip()
        for match in (
            re.match(r"DNG Illuminant (\d+): (.+)", line.strip()) for line in lines
        )
        if match
    }
    found = []
    for index, line in enumerate(lines):
        match = re.match(r"DNG color matrix (\d+):", line.strip())
        if match:
            rows = [
                [float(value) for value in lines[index + offset + 1].split()]
                for offset in range(3)
            ]
            found.append((illuminants[int(match.group(1))], rows))
    if found:
        return found
    for index, line in enumerate(lines):
        if line.strip() == "XYZ->CamRGB matrix:":
            rows = [
                [float(value) for value in lines[index + offset + 1].split()]
                for offset in range(3)
            ]
            return [("D65", rows)]
    raise ValueError("no colour matrices in raw-identify output")


def cct_of_xy(xy: np.ndarray) -> float:
    return float(uv_to_CCT_Robertson1968(xy_to_UCS_uv(xy))[0])


def xy_of_cct(cct: float) -> np.ndarray:
    from colour.temperature.robertson1968 import CCT_to_uv_Robertson1968

    return np.asarray(UCS_uv_to_xy(CCT_to_uv_Robertson1968([cct, 0.0])))


def interpolate(
    matrix_list: list[np.ndarray], temperatures: list[float], cct: float
) -> np.ndarray:
    if len(matrix_list) == 1:
        return matrix_list[0]
    inverse = 1.0 / cct
    fraction = (inverse - 1.0 / temperatures[1]) / (
        1.0 / temperatures[0] - 1.0 / temperatures[1]
    )
    fraction = min(max(fraction, 0.0), 1.0)
    return fraction * matrix_list[0] + (1.0 - fraction) * matrix_list[1]


def neutral_to_xy(neutral: np.ndarray, found) -> np.ndarray:
    matrix_list = [np.asarray(rows) for _, rows in found]
    temperatures = [ILLUMINANTS[name][0] for name, _ in found]
    order = np.argsort(temperatures)
    matrix_list = [matrix_list[index] for index in order]
    temperatures = [temperatures[index] for index in order]
    xy = np.asarray(ILLUMINANTS["D50"][1])
    for _ in range(30):
        matrix = interpolate(matrix_list, temperatures, cct_of_xy(xy))
        xyz = np.linalg.inv(matrix) @ neutral
        next_xy = np.asarray([xyz[0] / xyz.sum(), xyz[1] / xyz.sum()])
        if np.abs(next_xy - xy).sum() < 1e-7:
            return next_xy
        xy = next_xy
    return xy


def main() -> None:
    payload = {"meta": common.meta("gen_body.py"), "fixtures": {}}
    for name in FIXTURES:
        text = identify(common.ROOT / "fixtures" / name)
        coefficients = as_shot_coefficients(text)
        neutral = np.asarray([1.0 / coefficients[0], 1.0 / coefficients[1], 1.0 / coefficients[2]])
        found = matrices(text)
        xy = neutral_to_xy(neutral, found)
        payload["fixtures"][name] = {
            "as_shot_wb": [
                coefficients[0] / coefficients[1],
                1.0,
                coefficients[2] / coefficients[1],
            ],
            "cct_k": cct_of_xy(xy),
            "matrices": [
                {
                    "illuminant": illuminant,
                    "cct_k": ILLUMINANTS[illuminant][0],
                    "xyz_to_camera": rows,
                }
                for illuminant, rows in found
            ],
            "source": "libraw raw-identify -v; DNG NeutralToXY with colour-science Robertson 1968",
            "xy": common.floats(xy),
        }
    common.write_json(
        common.ROOT / "crates/looklab-body/tests/golden/fixtures_cct.json", payload
    )


if __name__ == "__main__":
    main()
