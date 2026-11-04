// SPDX-License-Identifier: AGPL-3.0-or-later
//! Prior chart error per source on a synthetic body with a known spectral
//! sensitivity (docs/09 P4.1).

use looklab_body::{
    BodyId, CalibrationIlluminant, CameraProfile, Cct, ChartError, ChartReference, InputTransform,
    SpectralSensitivity, SsfIlluminant, TransformVersion, camera_responses, chart_error,
    prior_from_database, prior_from_dng, prior_from_ssf, synthetic_sensitivity, training_data,
};
use looklab_color::cat::adaptation_matrix;
use looklab_color::xyy::{Illuminant, xy_to_xyz};
use looklab_color::{Cat, Mat3};
use looklab_samples::PatchId;

fn body() -> BodyId {
    BodyId::new("synthetic/body").expect("non-empty")
}

fn known_sensitivity() -> SpectralSensitivity {
    synthetic_sensitivity(training_data().grid(), [610.0, 545.0, 455.0], 45.0)
}

fn observations() -> (Vec<PatchId>, Vec<[f64; 3]>) {
    let data = training_data();
    let responses =
        camera_responses(&known_sensitivity(), SsfIlluminant::D65, data).expect("positive white");
    let ids = (0..responses.len())
        .map(|index| PatchId::new(index as u16))
        .collect();
    (ids, responses)
}

/// The ColorMatrix that reproduces `a0` at `illuminant`'s white.
fn color_matrix(a0: &Mat3, illuminant: CalibrationIlluminant) -> Mat3 {
    let white = xy_to_xyz(illuminant.xy());
    adaptation_matrix(Cat::Bradford, Illuminant::D65.xyz(), white)
        .mul(a0)
        .inverse()
        .expect("the prior matrix is invertible")
}

fn report(source: &str, transform: &InputTransform) -> ChartError {
    let (ids, rgb) = observations();
    let reference = ChartReference::colorchecker24_d65();
    let error = chart_error(transform, Cct::D65, &ids, &rgb, &reference)
        .expect("the chart error is finite");
    println!(
        "{source}: mean {:.3} median {:.3} p95 {:.3} max {:.3}",
        error.mean, error.median, error.p95, error.max
    );
    error
}

#[test]
fn prior_chart_error_per_source() {
    let data = training_data();
    let sensitivity = known_sensitivity();
    let ssf_prior = prior_from_ssf(&sensitivity, data).expect("spectral prior");
    let ssf = report(
        "ssf",
        &InputTransform::prior(body(), TransformVersion::new(1), ssf_prior),
    );

    let profile = CameraProfile::new(
        vec![
            (
                CalibrationIlluminant::A,
                color_matrix(&ssf_prior.a0_a, CalibrationIlluminant::A),
            ),
            (
                CalibrationIlluminant::D65,
                color_matrix(&ssf_prior.a0_d65, CalibrationIlluminant::D65),
            ),
        ],
        Vec::new(),
    )
    .expect("two calibration illuminants");
    let dng_prior = prior_from_dng(&profile).expect("dng prior");
    let dng = report(
        "dng",
        &InputTransform::prior(body(), TransformVersion::new(1), dng_prior),
    );

    let single = CameraProfile::new(
        vec![(
            CalibrationIlluminant::A,
            color_matrix(&ssf_prior.a0_a, CalibrationIlluminant::A),
        )],
        Vec::new(),
    )
    .expect("one calibration illuminant");
    let db_prior = prior_from_database(&single).expect("database prior");
    let db = report(
        "db",
        &InputTransform::prior(body(), TransformVersion::new(1), db_prior),
    );

    assert!(ssf.median < 3.0);
    assert!((dng.median - ssf.median).abs() < 0.05);
    assert!(db.median > ssf.median);
    assert!(db.median > dng.median);
}
