#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Generate CAM16 and CAM16-UCS golden vectors from colour-science 0.4.7.

Run from the repo root:

    nix develop .#golden -c python3 tools/golden/gen_cam16.py

Deterministic: fixed numpy seed, sorted JSON keys, full float precision.
Reference conditions use `discount_illuminant=True` (AGENTS.md, docs/02 §8).
"""

from __future__ import annotations

import json
from pathlib import Path

import colour
import numpy as np

SEED = 20261104
ROOT = Path(__file__).resolve().parents[2]
OUT = ROOT / "crates/looklab-color/tests/golden"

D65 = colour.CCS_ILLUMINANTS["CIE 1931 2 Degree Standard Observer"]["D65"]
D50 = colour.CCS_ILLUMINANTS["CIE 1931 2 Degree Standard Observer"]["D50"]

# `white` is the adopted white at Y = 1, `y_b` and `l_a` as colour-science takes them.
CONDITIONS = {
    "reference": {
        "white": colour.xy_to_XYZ(D65),
        "l_a": 64.0 / np.pi * 0.2,
        "y_b": 20.0,
        "surround": "Average",
        "discount_illuminant": True,
    },
    "average_no_discount": {
        "white": colour.xy_to_XYZ(D65),
        "l_a": 64.0 / np.pi * 0.2,
        "y_b": 20.0,
        "surround": "Average",
        "discount_illuminant": False,
    },
    "dim": {
        "white": colour.xy_to_XYZ(D65),
        "l_a": 100.0,
        "y_b": 20.0,
        "surround": "Dim",
        "discount_illuminant": False,
    },
    "dark": {
        "white": colour.xy_to_XYZ(D65),
        "l_a": 10.0,
        "y_b": 20.0,
        "surround": "Dark",
        "discount_illuminant": False,
    },
    "d50_average": {
        "white": colour.xy_to_XYZ(D50),
        "l_a": 64.0,
        "y_b": 20.0,
        "surround": "Average",
        "discount_illuminant": False,
    },
}

SPACES = {
    "Ucs": colour.JMh_CAM16_to_CAM16UCS,
    "Lcd": colour.JMh_CAM16_to_CAM16LCD,
    "Scd": colour.JMh_CAM16_to_CAM16SCD,
}

INV_SPACES = {
    "Ucs": colour.CAM16UCS_to_JMh_CAM16,
    "Lcd": colour.CAM16LCD_to_JMh_CAM16,
    "Scd": colour.CAM16SCD_to_JMh_CAM16,
}

K_L = {"Ucs": 1.00, "Lcd": 0.77, "Scd": 1.24}


def floats(values) -> list[float]:
    return [float(v) for v in np.asarray(values, dtype=float).ravel()]


def condition_arguments(condition: dict):
    return (
        np.asarray(condition["white"], dtype=float) * 100.0,
        condition["l_a"],
        condition["y_b"],
        colour.VIEWING_CONDITIONS_CAM16[condition["surround"]],
        condition["discount_illuminant"],
    )


def cam16(xyz, condition: dict):
    xyz_w, l_a, y_b, surround, discount = condition_arguments(condition)
    return colour.XYZ_to_CAM16(
        np.asarray(xyz, dtype=float) * 100.0,
        xyz_w,
        l_a,
        y_b,
        surround,
        discount_illuminant=discount,
    )


def cam16_to_xyz(j, chroma, h, condition: dict):
    xyz_w, l_a, y_b, surround, discount = condition_arguments(condition)
    if "m" in chroma:
        specification = colour.CAM_Specification_CAM16(J=j, M=chroma["m"], h=h)
    else:
        specification = colour.CAM_Specification_CAM16(J=j, C=chroma["c"], h=h)
    xyz = colour.CAM16_to_XYZ(specification, xyz_w, l_a, y_b, surround, discount)
    return np.asarray(xyz, dtype=float) / 100.0


def emit(name: str, data: dict) -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    text = json.dumps(data, sort_keys=True, indent=2, allow_nan=False) + "\n"
    (OUT / name).write_text(text)


def main() -> None:
    rng = np.random.default_rng(SEED)

    fixed_xyz = [
        np.array([0.9504559270516716, 1.0, 1.0890577507598784]) * y for y in (0.18, 1.0)
    ] + [
        np.array([0.3, 0.4, 0.5]),
        np.array([0.9, 0.2, 0.1]),
        np.array([0.05, 0.4, 0.05]),
        np.array([0.02, 0.02, 0.02]),
        np.array([0.2, 0.25, 0.6]),
        # Negative colourfulness: exercises the signed power in saturation.
        np.array([0.0, 0.1, -1.0]),
    ]

    forward = []
    ucs_j = []
    for name, condition in CONDITIONS.items():
        samples = list(fixed_xyz) + [
            rng.uniform(0.001, 1.0, size=3) for _ in range(8)
        ]
        for xyz in samples:
            cam = cam16(xyz, condition)
            forward.append(
                {
                    "condition": name,
                    "xyz": floats(xyz),
                    "j": float(cam.J),
                    "c": float(cam.C),
                    "h": float(cam.h),
                    "m": float(cam.M),
                    "s": float(cam.s),
                    "q": float(cam.Q),
                }
            )

        for _ in range(4):
            chromaticity = rng.uniform(0.05, 1.0, size=3)
            y = float(rng.uniform(0.01, 1.5))
            xyz = chromaticity / chromaticity[1] * y
            cam = cam16(xyz, condition)
            jmh = np.array([cam.J, cam.M, cam.h])
            ucs_j.append(
                {
                    "condition": name,
                    "xyz_at_unit_y": floats(chromaticity / chromaticity[1]),
                    "y": y,
                    "j_prime": float(colour.JMh_CAM16_to_CAM16UCS(jmh)[0]),
                }
            )

    emit(
        "cam16_forward.json",
        {
            "conditions": {
                name: {
                    "white": floats(condition["white"]),
                    "l_a": float(condition["l_a"]),
                    "y_b": float(condition["y_b"]),
                    "surround": condition["surround"],
                    "discount_illuminant": condition["discount_illuminant"],
                }
                for name, condition in CONDITIONS.items()
            },
            "forward": forward,
            "ucs_j": ucs_j,
        },
    )

    inverse = []
    for name, condition in CONDITIONS.items():
        for _ in range(6):
            j = float(rng.uniform(1.0, 99.0))
            m = float(rng.uniform(0.5, 80.0))
            c = float(rng.uniform(0.5, 80.0))
            h = float(rng.uniform(0.0, 360.0))
            inverse.append(
                {
                    "condition": name,
                    "j": j,
                    "m": m,
                    "c": c,
                    "h": h,
                    "xyz_m": floats(cam16_to_xyz(j, {"m": m}, h, condition)),
                    "xyz_c": floats(cam16_to_xyz(j, {"c": c}, h, condition)),
                }
            )
        for j, h in ((50.0, 0.0), (50.0, 123.0)):
            inverse.append(
                {
                    "condition": name,
                    "j": j,
                    "m": 0.0,
                    "c": 0.0,
                    "h": h,
                    "xyz_m": floats(cam16_to_xyz(j, {"m": 0.0}, h, condition)),
                    "xyz_c": floats(cam16_to_xyz(j, {"c": 0.0}, h, condition)),
                }
            )
    emit("cam16_inverse.json", {"inverse": inverse})

    ucs_forward = []
    ucs_inverse = []
    delta_e = []
    jmh_samples = [
        (rng.uniform(0.5, 99.0), rng.uniform(0.0, 90.0), rng.uniform(0.0, 360.0))
        for _ in range(12)
    ] + [(50.0, 0.0, 0.0), (100.0, 100.0, 180.0), (1.0, 1.0, 270.0)]
    for space, function in SPACES.items():
        previous = None
        for j, m, h in jmh_samples:
            jmh = np.array([j, m, h])
            papbp = function(jmh)
            ucs_forward.append(
                {
                    "space": space,
                    "jmh": floats(jmh),
                    "ucs": floats(papbp),
                }
            )
            jmh_back = INV_SPACES[space](papbp)
            ucs_inverse.append(
                {
                    "space": space,
                    "ucs": floats(papbp),
                    "jmh": floats(jmh_back),
                }
            )
            if previous is not None:
                de = np.sqrt(
                    ((papbp[0] - previous[0]) / K_L[space]) ** 2
                    + (papbp[1] - previous[1]) ** 2
                    + (papbp[2] - previous[2]) ** 2
                )
                delta_e.append(
                    {
                        "space": space,
                        "a": floats(papbp),
                        "b": floats(previous),
                        "de": float(de),
                    }
                )
            previous = papbp

    emit(
        "cam16_ucs.json",
        {"forward": ucs_forward, "inverse": ucs_inverse, "delta_e": delta_e},
    )


if __name__ == "__main__":
    main()
