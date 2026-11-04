// SPDX-License-Identifier: AGPL-3.0-or-later
//! Evidence from leave-one-out coverage.

use looklab_trust::{EvidenceGrid, TrustParams, coverage_gain, evidence};

fn grid_with(samples: &[(f64, f64, f64)]) -> EvidenceGrid {
    let mut grid = EvidenceGrid::default();
    for &(s, hue, chroma) in samples {
        grid.add(s, hue, chroma, 1.0).expect("finite sample");
    }
    grid
}

#[test]
fn identical_distributions_have_no_gain() {
    let samples = [(0.0, 10.0, 1.0), (1.0, 40.0, 2.0)];
    let gain = coverage_gain(&grid_with(&samples), &grid_with(&samples)).expect("same grid");
    assert!(gain.abs() < 1e-12);
}

#[test]
fn disjoint_support_has_full_gain() {
    let rest = grid_with(&[(0.0, 10.0, 1.0)]);
    let image = grid_with(&[(3.0, 200.0, 6.0)]);
    let gain = coverage_gain(&rest, &image).expect("same grid");
    assert!((gain - 1.0).abs() < 1e-12);
}

#[test]
fn an_empty_rest_gives_full_gain() {
    let gain =
        coverage_gain(&EvidenceGrid::default(), &grid_with(&[(0.0, 0.0, 1.0)])).expect("same grid");
    assert!((gain - 1.0).abs() < 1e-12);
}

#[test]
fn an_empty_image_has_no_gain() {
    let gain =
        coverage_gain(&grid_with(&[(0.0, 0.0, 1.0)]), &EvidenceGrid::default()).expect("same grid");
    assert!(gain.abs() < 1e-12);
}

#[test]
fn mismatched_grids_are_rejected() {
    let coarse = EvidenceGrid::new(4, 4, 4).expect("non-zero bins");
    let fine = EvidenceGrid::default();
    assert!(coverage_gain(&coarse, &fine).is_err());
}

#[test]
fn non_finite_samples_are_rejected() {
    let mut grid = EvidenceGrid::default();
    assert!(grid.add(f64::NAN, 0.0, 1.0, 1.0).is_err());
    assert!(grid.add(0.0, 0.0, 1.0, -1.0).is_err());
}

#[test]
fn evidence_clamps_at_the_configured_bounds() {
    let params = TrustParams::default();
    assert!((evidence(0.0, 0.0, &params) - 0.1).abs() < 1e-12);
    assert!((evidence(0.25, 0.0, &params) - 0.5).abs() < 1e-12);
    assert!((evidence(1.0, 0.0, &params) - 1.0).abs() < 1e-12);
    assert!((evidence(1.0, 0.5, &params) - 0.5).abs() < 1e-12);
    assert!((evidence(1.0, 1.0, &params) - 0.1).abs() < 1e-12);
    assert!((evidence(2.0, 0.0, &params) - 1.0).abs() < 1e-12);
}
