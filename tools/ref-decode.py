#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Reference reduced-size decode with LibRaw, for looklab-raw fixture tests.

Dumps the raw mosaic with LibRaw's `unprocessed_raw`, block-averages it like `looklab_raw`,
and writes per-channel means plus an 8x8 grid of block means as JSON:

    nix shell nixpkgs#libraw nixpkgs#python3 -c \\
        python3 tools/ref-decode.py --file fixtures/fujifilm-x-s10.raf \\
        --pattern GGRGGBGGBGGRBRGRBGGGBGGRGGRGGBRBGBRG --tile 6 \\
        --crop 0,5,6252,4176 --black 1022 --white 16382 \\
        --out crates/looklab-raw/tests/golden/fujifilm-x-s10.json
"""

import argparse
import array
import json
import os
import subprocess
import sys
import tempfile

GRID = 8


def read_pgm(path):
    with open(path, "rb") as fh:
        header = b""
        while header.count(b"\n") < 3:
            header += fh.read(1)
        magic, width, height, maxval = header.split()
        if magic != b"P5" or maxval != b"65535":
            raise SystemExit(f"{path}: expected a 16-bit P5 dump, got {header!r}")
        width, height = int(width), int(height)
        samples = array.array("H")
        samples.frombytes(fh.read(width * height * 2))
    if sys.byteorder == "little":
        samples.byteswap()
    return width, height, samples


def raw_dump(path):
    with tempfile.TemporaryDirectory() as tmp:
        link = os.path.join(tmp, os.path.basename(path))
        os.symlink(os.path.abspath(path), link)
        subprocess.run(["unprocessed_raw", link], check=True, capture_output=True)
        return read_pgm(link + ".pgm")


def reduce(width, samples, pattern, tile, crop, black, white):
    x0, y0, crop_w, crop_h = crop
    pattern_w, pattern_h = (2, 2) if len(pattern) == 4 else (6, 6)
    out_w, out_h = crop_w // tile, crop_h // tile
    planes = [[], [], []]
    for by in range(out_h):
        for bx in range(out_w):
            sums = [0.0, 0.0, 0.0]
            counts = [0, 0, 0]
            for y in range(y0 + by * tile, y0 + (by + 1) * tile):
                for x in range(x0 + bx * tile, x0 + (bx + 1) * tile):
                    color = pattern[(y % pattern_h) * pattern_w + x % pattern_w]
                    channel = {"R": 0, "G": 1, "B": 2}[color]
                    value = (samples[y * width + x] - black) / (white - black)
                    sums[channel] += max(value, 0.0)
                    counts[channel] += 1
            for channel in range(3):
                planes[channel].append(sums[channel] / counts[channel] if counts[channel] else 0.0)
    return out_w, out_h, planes


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--file", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--pattern", required=True, help="CFA string from raw-identify")
    parser.add_argument("--tile", type=int, required=True)
    parser.add_argument("--crop", required=True, help="x,y,width,height in the raw dump")
    parser.add_argument("--black", type=float, required=True)
    parser.add_argument("--white", type=float, required=True)
    args = parser.parse_args()
    crop = [int(part) for part in args.crop.split(",")]

    width, _height, samples = raw_dump(args.file)
    out_w, out_h, planes = reduce(
        width, samples, args.pattern, args.tile, crop, args.black, args.white
    )

    grid = []
    for gy in range(GRID):
        row = []
        for gx in range(GRID):
            x_start = gx * out_w // GRID
            x_end = (gx + 1) * out_w // GRID
            y_start = gy * out_h // GRID
            y_end = (gy + 1) * out_h // GRID
            cell = []
            for channel in range(3):
                values = [
                    planes[channel][y * out_w + x]
                    for y in range(y_start, y_end)
                    for x in range(x_start, x_end)
                ]
                cell.append(sum(values) / len(values))
            row.append(cell)
        grid.append(row)

    document = {
        "decoder": "libraw unprocessed_raw plus looklab block averaging",
        "pattern": args.pattern,
        "tile": args.tile,
        "crop": crop,
        "black": args.black,
        "white": args.white,
        "reduced": [out_w, out_h],
        "mean": [sum(plane) / len(plane) for plane in planes],
        "grid": grid,
    }
    with open(args.out, "w") as fh:
        json.dump(document, fh, indent=1, sort_keys=True)
        fh.write("\n")


if __name__ == "__main__":
    main()
