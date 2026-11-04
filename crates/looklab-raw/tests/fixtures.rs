// SPDX-License-Identifier: AGPL-3.0-or-later
//! Fixture decodes and the LibRaw reference comparison.
//! Fixtures are CC0 samples from raw.pixls.us; see fixtures/manifest.toml.

// Integration tests read fixture files; the library itself has no I/O.
#![allow(clippy::disallowed_methods)]

use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder, digest_f32};
use serde::Deserialize;
use std::path::PathBuf;

const GRID: usize = 8;
const TOLERANCE: f64 = 0.005;

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

fn decode_committed(name: &str) -> DecodedRaw {
    let path = fixture_path(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
    RawlerDecoder
        .decode(&bytes)
        .unwrap_or_else(|err| panic!("decode {}: {err}", path.display()))
}

fn size(img: &DecodedRaw) -> (usize, usize) {
    (img.width, img.height)
}

fn channel_means(img: &DecodedRaw) -> [f64; 3] {
    let pixels = (img.rgb.len() / 3) as f64;
    let mut sums = [0.0f64; 3];
    for (index, value) in img.rgb.iter().enumerate() {
        sums[index % 3] += f64::from(*value);
    }
    sums.map(|sum| sum / pixels)
}

fn channel_grid(img: &DecodedRaw) -> Vec<Vec<[f64; 3]>> {
    let mut grid = Vec::with_capacity(GRID);
    for gy in 0..GRID {
        let mut row = Vec::with_capacity(GRID);
        for gx in 0..GRID {
            let x_start = gx * img.width / GRID;
            let x_end = (gx + 1) * img.width / GRID;
            let y_start = gy * img.height / GRID;
            let y_end = (gy + 1) * img.height / GRID;
            let mut sums = [0.0f64; 3];
            let mut count = 0usize;
            for y in y_start..y_end {
                for x in x_start..x_end {
                    let pixel = img.pixel(x, y).expect("grid stays inside the image");
                    for channel in 0..3 {
                        sums[channel] += f64::from(pixel[channel]);
                    }
                    count += 1;
                }
            }
            row.push(sums.map(|sum| sum / count as f64));
        }
        grid.push(row);
    }
    grid
}

#[derive(Debug, Deserialize)]
struct Reference {
    tile: usize,
    crop: [usize; 4],
    reduced: [usize; 2],
    mean: [f64; 3],
    grid: Vec<Vec<[f64; 3]>>,
}

fn assert_close(actual: f64, expected: f64, context: &str) {
    let scale = actual.abs().max(expected.abs()).max(1e-6);
    assert!(
        (actual - expected).abs() <= TOLERANCE * scale,
        "{context}: {actual} vs reference {expected}"
    );
}

fn compare_with_reference(name: &str, json: &str) {
    let path = fixture_path(name);
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!(
            "skipping reference check for {name}: {} is absent; run tools/fetch-fixtures.sh",
            path.display()
        );
        return;
    };
    let reference: Reference = serde_json::from_str(json).expect("reference JSON");
    assert_eq!(reference.grid.len(), GRID);
    assert_eq!(reference.grid[0].len(), GRID);

    let img = RawlerDecoder.decode(&bytes).expect("decode fixture");
    assert_eq!(
        size(&img),
        (reference.reduced[0], reference.reduced[1]),
        "{name} reduced size"
    );
    assert_eq!(
        reference.reduced,
        [
            reference.crop[2] / reference.tile,
            reference.crop[3] / reference.tile
        ],
        "{name} reference geometry"
    );

    for channel in 0..3 {
        assert_close(
            channel_means(&img)[channel],
            reference.mean[channel],
            &format!("{name} mean channel {channel}"),
        );
    }
    for (gy, row) in channel_grid(&img).iter().enumerate() {
        for (gx, cell) in row.iter().enumerate() {
            for (channel, value) in cell.iter().enumerate() {
                assert_close(
                    *value,
                    reference.grid[gy][gx][channel],
                    &format!("{name} grid {gx},{gy} channel {channel}"),
                );
            }
        }
    }
}

#[test]
fn fujifilm_x_s10_digest() {
    let img = decode_committed("fujifilm-x-s10.raf");
    assert_eq!(size(&img), (1042, 696));
    assert_eq!(img.make, "FUJIFILM");
    assert_eq!(img.model, "X-S10");
    assert_eq!(img.as_shot_wb[1], 1.0);
    assert!(img.color_matrices.is_some());
    assert_eq!(digest_f32(&img.rgb), 0x14b1_973f_d0fd_c8ac);
}

#[test]
fn canon_eos_60d_digest() {
    let img = decode_committed("canon-eos-60d.dng");
    assert_eq!(size(&img), (480, 270));
    assert_eq!(img.make, "Canon");
    assert_eq!(img.model, "Canon EOS 60D");
    assert_eq!(img.as_shot_wb[1], 1.0);
    assert!(img.color_matrices.is_some());
    assert_eq!(digest_f32(&img.rgb), 0xc62f_5221_0dca_b14d);
}

#[test]
fn fujifilm_x_s10_matches_libraw() {
    compare_with_reference(
        "fujifilm-x-s10.raf",
        include_str!("golden/fujifilm-x-s10.json"),
    );
}

#[test]
fn canon_eos_60d_matches_libraw() {
    compare_with_reference(
        "canon-eos-60d.dng",
        include_str!("golden/canon-eos-60d.json"),
    );
}

#[test]
fn fetch_only_fixture_decodes_when_present() {
    let path = fixture_path("cache/sigma-fp-8bit.dng");
    let Ok(bytes) = std::fs::read(&path) else {
        eprintln!(
            "skipping fetch-only fixture check: {} is absent; run tools/fetch-fixtures.sh",
            path.display()
        );
        return;
    };
    let img = RawlerDecoder.decode(&bytes).expect("decode Sigma fp DNG");
    assert_eq!(size(&img), (968, 545));
    assert_eq!(img.make, "SIGMA");
}

#[test]
fn empty_input_is_rejected() {
    let err = RawlerDecoder.decode(&[]).expect_err("empty input");
    assert!(matches!(err, looklab_raw::RawError::EmptyInput));
}
