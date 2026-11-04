#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate golden vectors for looklab-color's colour-space basics.

Run from the repo root:

    nix develop .#golden -c python3 tools/golden/gen_basics.py

Writes ``crates/looklab-color/tests/golden/basics.json`` and
``data/charts/colorchecker24_2014.json``. Deterministic: fixed seed, sorted
keys, ``repr``-precision floats.
"""

from __future__ import annotations

import numpy as np

import colour
from colour.adaptation import (
    chromatic_adaptation_VonKries,
    matrix_chromatic_adaptation_VonKries,
)
from colour.difference import delta_E_CIE2000
from colour.models import (
    LCHab_to_Lab,
    Lab_to_LCHab,
    Lab_to_XYZ,
    XYZ_to_Lab,
    XYZ_to_xyY,
    xyY_to_XYZ,
    xy_to_XYZ,
)
from colour.models.rgb import (
    RGB_to_XYZ,
    XYZ_to_RGB,
    RGB_Colourspace,
    normalised_primary_matrix,
)
from colour.models.rgb.transfer_functions import (
    gamma,
    itur_bt_2020,
    rimm_romm_rgb,
    srgb,
)

import common

RANDOM = 200
ILLUMINANTS = colour.CCS_ILLUMINANTS["CIE 1931 2 Degree Standard Observer"]

SPACES = {
    "srgb": {
        "name": "sRGB",
        "colourspace": "sRGB",
        "primaries": [[0.640, 0.330], [0.300, 0.600], [0.150, 0.060]],
        "whitepoint": ILLUMINANTS["D65"],
        "encoding": srgb.eotf_inverse_sRGB,
        "decoding": srgb.eotf_sRGB,
    },
    "adobe_rgb": {
        "name": "Adobe RGB (1998)",
        "colourspace": "Adobe RGB (1998)",
        "primaries": [[0.640, 0.330], [0.210, 0.710], [0.150, 0.060]],
        "whitepoint": ILLUMINANTS["D65"],
        "encoding": lambda x: gamma.gamma_function(x, exponent=1 / 2.19921875),
        "decoding": lambda x: gamma.gamma_function(x, exponent=2.19921875),
    },
    "rec2020": {
        "name": "ITU-R BT.2020",
        "colourspace": "ITU-R BT.2020",
        "primaries": [[0.708, 0.292], [0.170, 0.797], [0.131, 0.046]],
        "whitepoint": ILLUMINANTS["D65"],
        "encoding": itur_bt_2020.oetf_BT2020,
        "decoding": itur_bt_2020.oetf_inverse_BT2020,
    },
    "display_p3": {
        "name": "Display P3",
        "colourspace": "Display P3",
        "primaries": [[0.680, 0.320], [0.265, 0.690], [0.150, 0.060]],
        "whitepoint": ILLUMINANTS["D65"],
        "encoding": srgb.eotf_inverse_sRGB,
        "decoding": srgb.eotf_sRGB,
    },
    "prophoto": {
        "name": "ProPhoto RGB",
        "colourspace": "ProPhoto RGB",
        "primaries": [[0.7347, 0.2653], [0.1596, 0.8404], [0.0366, 0.0001]],
        "whitepoint": ILLUMINANTS["D50"],
        "encoding": rimm_romm_rgb.cctf_encoding_ROMMRGB,
        "decoding": rimm_romm_rgb.cctf_decoding_ROMMRGB,
    },
}

CATS = {
    "cat02": "CAT02",
    "cat16": "CAT16",
    "bradford": "Bradford",
}

TRANSFER_FUNCTIONS = {
    "srgb_encode": srgb.eotf_inverse_sRGB,
    "srgb_decode": srgb.eotf_sRGB,
    "adobe_rgb_encode": SPACES["adobe_rgb"]["encoding"],
    "adobe_rgb_decode": SPACES["adobe_rgb"]["decoding"],
    "rec2020_encode": itur_bt_2020.oetf_BT2020,
    "rec2020_decode": itur_bt_2020.oetf_inverse_BT2020,
    "prophoto_encode": rimm_romm_rgb.cctf_encoding_ROMMRGB,
    "prophoto_decode": rimm_romm_rgb.cctf_decoding_ROMMRGB,
}

# Adobe RGB is a pure power law in colour-science, so negative input is NaN and
# cannot appear in a finite golden vector.
NON_NEGATIVE = {"adobe_rgb_encode", "adobe_rgb_decode"}


def derived_colourspace(spec: dict) -> RGB_Colourspace:
    primaries = np.asarray(spec["primaries"])
    whitepoint = np.asarray(spec["whitepoint"])
    matrix = normalised_primary_matrix(primaries, whitepoint)
    return RGB_Colourspace(
        spec["name"],
        primaries,
        whitepoint,
        None,
        matrix,
        np.linalg.inv(matrix),
        spec["encoding"],
        spec["decoding"],
    )


def scalar_rows(values) -> list[list[float]]:
    return [[float(value)] for value in values]


def white_rows(rng: np.random.Generator, count: int) -> np.ndarray:
    xy = white_chromaticities(rng, count)
    return np.asarray([xy_to_XYZ(row) for row in xy])


def white_chromaticities(rng: np.random.Generator, count: int) -> np.ndarray:
    return np.stack(
        [rng.uniform(0.15, 0.55, count), rng.uniform(0.20, 0.50, count)], axis=-1
    )


def main() -> None:
    rng = common.rng(1)
    vectors: dict[str, dict] = {}

    xyz = rng.uniform(-0.1, 1.5, (RANDOM, 3))
    xyz = np.vstack([xyz, [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0], [1e-12, 2e-12, 3e-12], [0.2, 0.4, 0.6]]])
    vectors["xyz_to_xyy"] = common.vectors(
        xyz, [XYZ_to_xyY(row) for row in xyz]
    )

    xyy = np.column_stack(
        [
            rng.uniform(-0.1, 1.2, RANDOM),
            rng.uniform(-0.2, 1.2, RANDOM),
            rng.uniform(-0.5, 1.5, RANDOM),
        ]
    )
    xyy = np.vstack([xyy, [[0.3, 0.0, 0.5], [0.0, 0.0, 0.0], [1 / 3, 1 / 3, 1.0], [0.6, 0.3, 0.5], [0.2, 0.5, 1.0]]])
    vectors["xyy_to_xyz"] = common.vectors(
        xyy, [xyY_to_XYZ(row) for row in xyy]
    )

    xy = rng.uniform(-0.1, 1.2, (RANDOM, 2))
    xy = np.vstack([xy, [[0.0, 0.0], [1 / 3, 1 / 3], [0.3127, 0.329], [0.3457, 0.3585], [0.64, 0.33]]])
    vectors["xy_to_xyz"] = common.vectors(xy, [xy_to_XYZ(row) for row in xy])

    for name, function in TRANSFER_FUNCTIONS.items():
        if name in NON_NEGATIVE:
            linear = np.append(rng.uniform(0.0, 1.5, RANDOM), [0.0, 1 / 512, 1 / 32, 0.018, 0.04045, 0.5, 1.0, 1.5])
            encoded = np.append(rng.uniform(0.0, 1.5, RANDOM), [0.0, 1 / 512, 1 / 32, 0.040449936, 0.08124794403514049, 0.5, 1.0, 1.5])
        else:
            linear = np.append(rng.uniform(-0.25, 1.5, RANDOM), [-0.25, 0.0, 1 / 512, 1 / 32, 0.0031308, 0.018, 0.04045, 0.5, 1.0, 1.5])
            encoded = np.append(rng.uniform(-0.25, 1.5, RANDOM), [-0.25, 0.0, 0.0031308, 0.040449936, 0.08124794403514049, 1 / 32, 0.5, 1.0, 1.5])
        source = encoded if name.endswith("decode") else linear
        vectors[name] = common.vectors(scalar_rows(source), scalar_rows(function(source)))

    rgb_inputs = np.vstack(
        [
            rng.uniform(-0.25, 1.5, (RANDOM, 3)),
            [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.18, 0.18, 0.18], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
        ]
    )
    spaces: dict[str, dict] = {}
    for key, spec in SPACES.items():
        colourspace = derived_colourspace(spec)
        spaces[key] = {
            "rgb_to_xyz": common.vectors(
                rgb_inputs,
                [RGB_to_XYZ(row, colourspace) for row in rgb_inputs],
            ),
            "xyz_to_rgb": common.vectors(
                xyz,
                [XYZ_to_RGB(row, colourspace) for row in xyz],
            ),
        }

    cats: dict[str, dict] = {}
    for key, transform in CATS.items():
        source = white_rows(rng, RANDOM)
        destination = white_rows(rng, RANDOM)
        source = np.vstack([source, [xy_to_XYZ(ILLUMINANTS["D65"]), xy_to_XYZ(ILLUMINANTS["E"])]])
        destination = np.vstack([destination, [xy_to_XYZ(ILLUMINANTS["D50"]), xy_to_XYZ(ILLUMINANTS["A"])]])
        matrix_inputs = np.column_stack([source, destination])
        adapt_inputs = np.column_stack([xyz[: len(source)], source, destination])
        cats[key] = {
            "matrix": common.vectors(
                matrix_inputs,
                [
                    matrix_chromatic_adaptation_VonKries(src, dst, transform)
                    for src, dst in zip(source, destination)
                ],
            ),
            "adapt": common.vectors(
                adapt_inputs,
                [
                    chromatic_adaptation_VonKries(row[:3], row[3:6], row[6:9], transform)
                    for row in adapt_inputs
                ],
            ),
        }

    lab_xyz = np.vstack([rng.uniform(-0.1, 1.2, (RANDOM, 3)), [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [0.2, 0.4, 0.6]]])
    lab_xy = white_chromaticities(np.random.default_rng([common.SEED, 2]), len(lab_xyz))
    lab_white = np.asarray([xy_to_XYZ(row) for row in lab_xy])
    lab_inputs = np.column_stack([lab_xyz, lab_white])
    vectors["xyz_to_lab"] = common.vectors(
        lab_inputs,
        [XYZ_to_Lab(row[:3], xy) for row, xy in zip(lab_inputs, lab_xy)],
    )

    lab_values = np.column_stack(
        [
            rng.uniform(-10.0, 110.0, RANDOM),
            rng.uniform(-120.0, 120.0, RANDOM),
            rng.uniform(-120.0, 120.0, RANDOM),
        ]
    )
    lab_values = np.vstack([lab_values, [[0.0, 0.0, 0.0], [100.0, 0.0, 0.0], [50.0, 2.5, 0.0], [50.0, -40.0, 30.0]]])
    lab_xy = white_chromaticities(np.random.default_rng([common.SEED, 3]), len(lab_values))
    white = np.asarray([xy_to_XYZ(row) for row in lab_xy])
    vectors["lab_to_xyz"] = common.vectors(
        np.column_stack([lab_values, white]),
        [Lab_to_XYZ(row, xy) for row, xy in zip(lab_values, lab_xy)],
    )

    vectors["lab_to_lchab"] = common.vectors(
        lab_values, [Lab_to_LCHab(row) for row in lab_values]
    )

    lchab = np.column_stack(
        [
            rng.uniform(-10.0, 110.0, RANDOM),
            rng.uniform(0.0, 150.0, RANDOM),
            rng.uniform(-720.0, 720.0, RANDOM),
        ]
    )
    lchab = np.vstack([lchab, [[50.0, 0.0, 0.0], [50.0, 20.0, 90.0], [50.0, 20.0, 180.0], [50.0, 20.0, 270.0], [50.0, 20.0, 359.0]]])
    vectors["lchab_to_lab"] = common.vectors(
        lchab, [LCHab_to_Lab(row) for row in lchab]
    )

    pair = np.column_stack([lab_values, lab_values[::-1]])
    vectors["delta_e_ciede2000"] = common.vectors(
        pair,
        [delta_E_CIE2000(row[:3], row[3:6]) for row in pair],
    )

    illuminants = np.asarray([ILLUMINANTS[key] for key in ("A", "D50", "D65", "E")])
    vectors["illuminant_xyz"] = common.vectors(
        illuminants, [xy_to_XYZ(row) for row in illuminants]
    )

    matrices = {}
    for key, spec in SPACES.items():
        primaries = np.asarray(spec["primaries"])
        whitepoint = np.asarray(spec["whitepoint"])
        matrix = normalised_primary_matrix(primaries, whitepoint)
        stored = colour.RGB_COLOURSPACES[spec["colourspace"]]
        matrices[key] = {
            "name": spec["name"],
            "primaries": common.floats(np.column_stack([primaries, np.ones(3)])),
            "whitepoint_xy": common.floats(whitepoint),
            "derived_rgb_to_xyz": common.floats(matrix),
            "derived_xyz_to_rgb": common.floats(np.linalg.inv(matrix)),
            "stored_rgb_to_xyz": common.floats(stored.matrix_RGB_to_XYZ),
            "stored_xyz_to_rgb": common.floats(stored.matrix_XYZ_to_RGB),
        }

    payload = common.meta("tools/golden/gen_basics.py")
    payload["vectors"] = vectors
    payload["spaces"] = spaces
    payload["cats"] = cats
    payload["space_matrices"] = matrices
    common.write_json(common.ROOT / "crates/looklab-color/tests/golden/basics.json", payload)

    checker = colour.CCS_COLOURCHECKERS["ColorChecker24 - After November 2014"]
    chart = {
        "colour_science_version": colour.__version__,
        "illuminant": "D50",
        "illuminant_xy": common.floats(checker.illuminant),
        "name": checker.name,
        "source": (
            "colour-science CCS_COLOURCHECKERS; BabelColor/X-Rite reference "
            "values, edition 'After November 2014'"
        ),
        "patches": [
            {"name": name, "xyy": common.floats(value)}
            for name, value in checker.data.items()
        ],
    }
    common.write_json(common.ROOT / "data/charts/colorchecker24_2014.json", chart)


if __name__ == "__main__":
    main()
