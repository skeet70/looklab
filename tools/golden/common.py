#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Shared plumbing for golden-vector scripts.

Vectors are byte-deterministic: a fixed seed, sorted JSON keys and
``repr``-precision floats.
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np

ROOT = Path(__file__).resolve().parents[2]
SEED = 20261104


def rng(stream: int = 0) -> np.random.Generator:
    """Generator for one deterministic stream of the shared seed."""
    return np.random.default_rng([SEED, stream])


def floats(values) -> list[float]:
    """Row-major float list, rejecting NaN and infinity."""
    array = np.asarray(values, dtype=np.float64)
    if not np.all(np.isfinite(array)):
        raise ValueError("golden vectors must be finite")
    return [float(value) for value in array.reshape(-1)]


def vectors(inputs, outputs) -> dict:
    return {
        "inputs": [floats(row) for row in inputs],
        "outputs": [floats(row) for row in outputs],
    }


def write_json(path: Path, payload) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    text = json.dumps(payload, indent=1, sort_keys=True, allow_nan=False)
    path.write_text(text + "\n", encoding="utf-8")


def meta(script: str) -> dict:
    import colour

    return {
        "colour_science_version": colour.__version__,
        "generated_by": script,
        "numpy_version": np.__version__,
        "seed": SEED,
    }
