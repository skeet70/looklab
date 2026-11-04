#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate the spectral reference data in ``data/charts/``.

Writes ``data/charts/colorchecker24_spectral.json``: the ColorChecker 24
reflectances (colour-science ``SDS_COLOURCHECKERS["BabelColor Average"]``), the
CIE 1931 2 degree standard observer colour matching functions, and the A and
D65 illuminant spectral power distributions, all sampled on one 5 nm grid from
380 nm to 730 nm.

The Rust crate ``looklab-body`` integrates these with a camera's spectral
sensitivity to derive input-transform priors (docs/02 §2). Reflectance values
are BabelColor/X-Rite measurements, CMFs are CIE 018:2019, and the illuminant
SPDs are CIE 015:2018.
"""

from __future__ import annotations

import numpy as np

import colour
from colour.characterisation import SDS_COLOURCHECKERS

import common

GRID = np.arange(380.0, 731.0, 5.0)


def resample(sd) -> np.ndarray:
    """``sd`` sampled on :data:`GRID` by linear interpolation."""
    return np.interp(GRID, sd.wavelengths, sd.values)


def main() -> None:
    cmfs = colour.MSDS_CMFS["CIE 1931 2 Degree Standard Observer"]
    illuminants = {name: colour.SDS_ILLUMINANTS[name] for name in ("A", "D65")}
    checker = SDS_COLOURCHECKERS["BabelColor Average"]
    names = list(checker.keys())

    payload = {
        "meta": {
            **common.meta("gen_spectral.py"),
            "chart": "ColorChecker24 - BabelColor Average",
            "source": (
                "colour-science SDS_COLOURCHECKERS, MSDS_CMFS and SDS_ILLUMINANTS; "
                "BabelColor/X-Rite reflectances, CIE 1931 2 degree observer, "
                "CIE illuminants A and D65"
            ),
            "notice": (
                "BabelColor ColorChecker data: Copyright (C) 2004-2012 Danny "
                "Pascale (www.babelcolor.com); used by permission"
            ),
            "grid_step_nm": float(GRID[1] - GRID[0]),
        },
        "grid_nm": common.floats(GRID),
        "cmfs": {
            channel: common.floats(np.interp(GRID, cmfs.wavelengths, cmfs.values[:, index]))
            for index, channel in enumerate(("x", "y", "z"))
        },
        "illuminants": {
            name.lower(): common.floats(resample(sd))
            for name, sd in illuminants.items()
        },
        "reflectances": {
            "chart": "ColorChecker24 - BabelColor Average",
            "patches": [
                {"name": name, "values": common.floats(resample(checker[name]))}
                for name in names
            ],
        },
    }
    common.write_json(common.ROOT / "data/charts/colorchecker24_spectral.json", payload)


if __name__ == "__main__":
    main()
