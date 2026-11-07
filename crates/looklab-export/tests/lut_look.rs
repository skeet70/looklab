// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::{Rgb, RgbSpace};
use looklab_export::card::{CardError, CardMeta, CardOptions, read_card, write_card};
use looklab_export::{
    CubeFile, Lut3d, LutLookOptions, definition_delta, import_lut_look, lut_data_from_cube,
    read_cube, ucs_to_xyz, write_cube,
};
use looklab_match::{Emphasis, MatchError, PreparedTarget, TargetSpec};
use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, DeclaredEncoding, Gamut, LicenseRecord, LogTransfer,
    LookVersion, LutData, LutDefinition, LutRecord, OutputEncoding, Slug,
};
use looklab_model::{
    ClipFunction, ClipSettings, Density, Look, LookDocument, LookIdentity, LookKind,
    ProbeCovariance, ProbeId, ProbeScalars, Tone, UncertaintyData, WbPolicy, neutral_j_prime,
    probes,
};

fn identity(slug: &str) -> LookIdentity {
    LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive).expect("identity")
}

fn cube() -> CubeFile {
    let mut data = Vec::new();
    for b in 0..3 {
        for g in 0..3 {
            for r in 0..3 {
                let [r, g, b] = [r as f32 / 2.0, g as f32 / 2.0, b as f32 / 2.0];
                data.push([
                    0.85 * r + 0.10 * g + 0.03 * b,
                    0.05 * r + 0.80 * g + 0.10 * b,
                    0.02 * r + 0.08 * g + 0.75 * b,
                ]);
            }
        }
    }
    CubeFile {
        lut: Lut3d {
            size: 3,
            data,
            shaper: None,
        },
        domain: [0.0, 1.0],
    }
}

fn license() -> LicenseRecord {
    LicenseRecord {
        own_work: false,
        source_license: Some("CC0-1.0".to_string()),
        source_url: None,
        attribution: "a donor".to_string(),
    }
}

fn definition() -> LutDefinition {
    LutDefinition {
        name: "Test Look".to_string(),
        license: license(),
        encoding: DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        },
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: 3,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
        },
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    }
}

fn options() -> LutLookOptions {
    LutLookOptions {
        max_sweeps: 2,
        hessian_samples: 500,
        ..LutLookOptions::default()
    }
}

#[test]
fn import_is_deterministic_and_round_trips() {
    let cube = cube();
    let first = import_lut_look(identity("test-look"), definition(), &cube, None, options())
        .expect("import");
    let second = import_lut_look(identity("test-look"), definition(), &cube, None, options())
        .expect("import");
    assert_eq!(
        first.document.encode().expect("encode"),
        second.document.encode().expect("encode")
    );
    assert_eq!(first.approximation, second.approximation);
    let bytes = first.document.encode().expect("encode");
    let decoded = looklab_model::LookDocument::decode(&bytes).expect("decode");
    assert_eq!(decoded.lut, first.document.lut);
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
    assert!(first.approximation.probes > 0);
    assert!(first.approximation.p95.is_finite());
}

#[test]
fn a_display_base_is_pending_until_pinned() {
    let cube = cube();
    let mut pending = definition();
    pending.encoding = DeclaredEncoding::DisplayReferred {
        base: BaseReference::neutral_baseline(),
        status: BaseStatus::BaseIncomplete {
            missing: BaseField::FilmSimulation,
        },
    };
    let imported =
        import_lut_look(identity("pending-look"), pending, &cube, None, options()).expect("import");
    assert!(!imported.document.is_matchable());
    assert!(matches!(
        write_card(
            &imported.document,
            &CardMeta::new("Test", "Test"),
            &CardOptions::default()
        ),
        Err(CardError::NotMatchable(
            looklab_model::MatchRefusal::DisplayBaseUnpinned
        ))
    ));

    let mut pinned = definition();
    pinned.encoding = DeclaredEncoding::DisplayReferred {
        base: BaseReference::neutral_baseline(),
        status: BaseStatus::Pinned,
    };
    let imported = import_lut_look(
        identity("pinned-look"),
        pinned,
        &cube,
        None,
        LutLookOptions::default(),
    )
    .expect("import");
    assert!(imported.document.is_matchable());
    assert!(
        write_card(
            &imported.document,
            &CardMeta::new("Test", "Test"),
            &CardOptions::default()
        )
        .is_ok()
    );
}

#[test]
fn definition_delta_of_a_look_with_itself_is_zero() {
    let cube = cube();
    let imported = import_lut_look(identity("same-look"), definition(), &cube, None, options())
        .expect("import");
    let delta =
        definition_delta(&imported.document, None, &imported.document, None).expect("delta");
    assert_eq!(delta.max, 0.0);
    assert_eq!(delta.p95, 0.0);
    assert!(delta.probes > 0);
}

#[test]
fn a_stored_lut_with_a_non_finite_value_is_rejected() {
    let mut data = cube();
    data.lut.data[0][0] = f32::NAN;
    let values: Vec<f64> = data
        .lut
        .data
        .iter()
        .flat_map(|rgb| rgb.iter().map(|value| *value as f64))
        .collect();
    let _ = LutData::new(3, [0.0; 3], [1.0; 3], values).expect_err("non-finite LUT");
}

fn bt709_decode(code: f64) -> f64 {
    if code < 0.081 {
        code / 4.5
    } else {
        libm::pow((code + 0.099) / 1.099, 1.0 / 0.45)
    }
}

fn layered_look() -> Look {
    let mut look = Look::colorimetric_spline().expect("tone spline");
    if let Tone::Spline { c0, beta } = &mut look.tone {
        *c0 *= 1.06;
        for coefficient in beta.iter_mut() {
            *coefficient *= 1.03;
        }
    }
    look.tint.a.fill(0.5);
    look.tint.b.fill(-0.3);
    look.color.g.plane.fill(0.02);
    look.color.r.plane.fill(0.5);
    look.color.lambda.plane.fill(0.15);
    look.clip = ClipSettings {
        function: ClipFunction::TanhSoftV1,
        k: 0.005,
    };
    look
}

fn synthetic_cube(look: &Look) -> CubeFile {
    const SIZE: usize = 33;
    let mut data = Vec::with_capacity(SIZE * SIZE * SIZE);
    for b in 0..SIZE {
        for g in 0..SIZE {
            for r in 0..SIZE {
                let code = [r, g, b].map(|index| index as f64 / (SIZE - 1) as f64);
                let linear = Rgb::new(
                    bt709_decode(code[0]),
                    bt709_decode(code[1]),
                    bt709_decode(code[2]),
                );
                let xyz = RgbSpace::Srgb.rgb_linear_to_xyz(linear);
                let post = look.output_encode(look.eval(xyz), RgbSpace::Srgb);
                let srgb = RgbSpace::Srgb.xyz_to_rgb_linear(ucs_to_xyz(post));
                data.push([
                    RgbSpace::Srgb.encode(srgb.r) as f32,
                    RgbSpace::Srgb.encode(srgb.g) as f32,
                    RgbSpace::Srgb.encode(srgb.b) as f32,
                ]);
            }
        }
    }
    CubeFile {
        lut: Lut3d {
            size: SIZE as u32,
            data,
            shaper: None,
        },
        domain: [0.0, 1.0],
    }
}

fn synthetic_definition(cube: &CubeFile) -> LutDefinition {
    let lut = lut_data_from_cube(cube).expect("lut");
    LutDefinition {
        name: "Synthetic".to_string(),
        license: license(),
        encoding: DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status: BaseStatus::Pinned,
        },
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    }
}

#[test]
fn a_synthetic_lut_from_a_layered_look_is_recovered() {
    let generated = synthetic_cube(&layered_look());
    let text = write_cube(&generated.lut, "synthetic", [0.0, 1.0]);
    let cube = read_cube(&text).expect("cube");
    let imported = import_lut_look(
        identity("synthetic-recovery"),
        synthetic_definition(&cube),
        &cube,
        None,
        LutLookOptions::default(),
    )
    .expect("import");
    assert!(imported.supported > 300, "supported {}", imported.supported);
    assert!(
        imported.approximation.p95 <= 0.5,
        "supported-probe p95 {:.3} on {} probes",
        imported.approximation.p95,
        imported.supported
    );
}

fn lowered_white_look() -> Look {
    const PLATEAU_Y: f64 = 0.793_361_860_471;
    let s_plateau = libm::log2(PLATEAU_Y / 0.18);
    let mut s = Vec::new();
    let mut j_prime = Vec::new();
    for step in 0..=240 {
        let value = -10.0 + 15.5 * step as f64 / 240.0;
        s.push(value);
        j_prime.push(neutral_j_prime(0.18 * libm::pow(2.0, value.min(s_plateau))));
    }
    let mut look = Look::colorimetric();
    look.tone = Tone::from_neutral_samples(&s, &j_prime).expect("monotone tone");
    look
}

#[test]
fn a_lowered_white_lut_imports_within_the_gate() {
    let generated = synthetic_cube(&lowered_white_look());
    let white = generated.lut.data.last().expect("white node");
    assert!(
        (white[0] - 0.903).abs() < 0.01 && (white[1] - 0.903).abs() < 0.01,
        "the control LUT must lower white, got {white:?}"
    );
    let text = write_cube(&generated.lut, "lowered-white", [0.0, 1.0]);
    let cube = read_cube(&text).expect("cube");
    let imported = import_lut_look(
        identity("lowered-white"),
        synthetic_definition(&cube),
        &cube,
        None,
        LutLookOptions::default(),
    )
    .expect("import");
    assert!(
        imported.approximation.p95 <= 1.0,
        "lowered-white p95 {:.3} on {} probes",
        imported.approximation.p95,
        imported.approximation.probes
    );
}

fn probe_index(id: ProbeId) -> usize {
    probes::v1()
        .probes
        .iter()
        .position(|probe| probe.id == id)
        .unwrap_or_else(|| panic!("probe {}", id.as_str()))
}

fn base_identity() -> LookIdentity {
    LookIdentity::new(
        "measured-base",
        "1.0.0",
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("base identity")
}

fn measured_base() -> LookDocument {
    let probe_set = probes::v1();
    let count = probe_set.probes.len();
    let uncertain = probe_index(ProbeId::new("gray-s-1.00"));
    let amplified = probe_index(ProbeId::new("gray-s-2.00"));
    let clipped = probe_index(ProbeId::new("gray-s+1.00"));
    let unsupported = probe_index(ProbeId::new("gray-s-3.00"));
    let mut covariance = vec![0.0_f64; count * 9];
    let mut sigma = vec![0.0_f64; count];
    let mut support = vec![1.0_f64; count];
    let mut reliable = vec![1.0_f64; count];
    let mut density = vec![1.0_f64; count];
    for index in 0..count {
        let variance = if index == uncertain { 9.0 } else { 0.0025 };
        covariance[index * 9] = variance;
        covariance[index * 9 + 4] = variance;
        covariance[index * 9 + 8] = variance;
        sigma[index] = (3.0 * variance).sqrt();
    }
    support[unsupported] = 0.0;
    reliable[clipped] = 0.0;
    density[amplified] = 0.42;
    let uncertainty = UncertaintyData {
        probe_cov_post: Some(ProbeCovariance::new(count, covariance).expect("covariance")),
        probe_sigma_post: Some(ProbeScalars::new(sigma).expect("sigma")),
        probe_pre_reliable: Some(ProbeScalars::new(reliable).expect("reliable")),
        support: Some(ProbeScalars::new(support).expect("support")),
        density: Some(Density::new(density).expect("density")),
        ..UncertaintyData::default()
    };
    LookDocument::from_look(base_identity(), layered_look())
        .expect("base document")
        .with_uncertainty(uncertainty)
        .expect("base uncertainty")
}

/// A LUT with slope 1.8 up to quarter code, steeper to mid code, flat above it.
fn steep_cube() -> CubeFile {
    const SIZE: usize = 5;
    let map = |code: f64| {
        if code <= 0.25 {
            1.8 * code
        } else if code <= 0.5 {
            0.45 + 2.2 * (code - 0.25)
        } else {
            1.0
        }
    };
    let mut data = Vec::new();
    for b in 0..SIZE {
        for g in 0..SIZE {
            for r in 0..SIZE {
                let [r, g, b] = [
                    map(r as f64 / (SIZE - 1) as f64),
                    map(g as f64 / (SIZE - 1) as f64),
                    map(b as f64 / (SIZE - 1) as f64),
                ];
                data.push([r as f32, g as f32, b as f32]);
            }
        }
    }
    CubeFile {
        lut: Lut3d {
            size: SIZE as u32,
            data,
            shaper: None,
        },
        domain: [0.0, 1.0],
    }
}

fn measured_definition(cube: &CubeFile) -> LutDefinition {
    let lut = lut_data_from_cube(cube).expect("lut");
    LutDefinition {
        name: "Measured".to_string(),
        license: license(),
        encoding: DeclaredEncoding::DisplayReferred {
            base: BaseReference::Look {
                slug: Slug::new("measured-base").expect("slug"),
                version: LookVersion::new("1.0.0").expect("version"),
            },
            status: BaseStatus::Pinned,
        },
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    }
}

#[test]
fn a_measured_base_inherits_uncertainty_through_the_lut() {
    let base = measured_base();
    let cube = steep_cube();
    let imported = import_lut_look(
        identity("measured-import"),
        measured_definition(&cube),
        &cube,
        Some(&base),
        LutLookOptions::default(),
    )
    .expect("import");
    let decoded =
        LookDocument::decode(&imported.document.encode().expect("encode")).expect("decode");
    let uncertainty = &decoded.uncertainty;
    let covariance = uncertainty
        .probe_cov_post
        .as_ref()
        .expect("post covariance");
    let sigma = uncertainty.probe_sigma_post.as_ref().expect("post sigma");
    let support = uncertainty.support.as_ref().expect("support");
    let reliable = uncertainty.probe_pre_reliable.as_ref().expect("reliable");
    let density = uncertainty.density.as_ref().expect("density");

    let uncertain = probe_index(ProbeId::new("gray-s-1.00"));
    let amplified = probe_index(ProbeId::new("gray-s-2.00"));
    let clipped = probe_index(ProbeId::new("gray-s+1.00"));
    let unsupported = probe_index(ProbeId::new("gray-s-3.00"));
    assert_eq!(covariance.probes, probes::v1().probes.len());
    assert_eq!(covariance.values.len(), covariance.probes * 9);
    assert_eq!(support.values[uncertain], 0.0);
    assert_eq!(support.values[unsupported], 0.0);
    assert!(sigma.values[uncertain] > 1.5);
    assert_eq!(reliable.values[clipped], 0.0);
    assert!(reliable.values.iter().any(|value| *value > 0.5));
    assert_eq!(density.values[amplified], 0.42);
    let base_sigma = base
        .uncertainty
        .probe_sigma_post
        .as_ref()
        .expect("base sigma");
    assert!(sigma.values[amplified] > base_sigma.values[amplified]);

    let supported: Vec<usize> = support
        .values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (*value > 0.5).then_some(index))
        .collect();
    assert!(!supported.is_empty());
    assert!(supported.len() < support.values.len());
    assert!(decoded.is_matchable());
    let probe_set = probes::v1();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probe_set,
        function: &decoded.look,
        uncertainty: Some(&decoded.uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: Some(0.01),
    })
    .expect("prepared target");
    assert_eq!(target.supported, supported);
    assert!(!target.supported.contains(&uncertain));
    assert!(!target.supported.contains(&unsupported));
    assert_eq!(target.weight[uncertain], 0.0);
    assert_eq!(target.weight[unsupported], 0.0);
    assert!(target.weight[supported[0]] > 0.0);
}

#[test]
fn a_measured_base_without_uncertainty_is_not_matchable() {
    let base = LookDocument::from_look(base_identity(), layered_look()).expect("base document");
    let cube = cube();
    let imported = import_lut_look(
        identity("missing-base-uncertainty"),
        measured_definition(&cube),
        &cube,
        Some(&base),
        options(),
    )
    .expect("import");
    let decoded =
        LookDocument::decode(&imported.document.encode().expect("encode")).expect("decode");
    let sigma = decoded
        .uncertainty
        .probe_sigma_post
        .as_ref()
        .expect("post sigma");
    assert!(sigma.values.iter().all(|value| *value > 1.5));
    let support = decoded.uncertainty.support.as_ref().expect("support");
    assert!(support.values.iter().all(|value| *value == 0.0));
    assert!(!decoded.is_matchable());
    let probe_set = probes::v1();
    assert!(matches!(
        PreparedTarget::new(TargetSpec {
            probes: &probe_set,
            function: &decoded.look,
            uncertainty: Some(&decoded.uncertainty),
            emphasis: Emphasis::Balanced,
            tau: 1.0,
            support_sigma: 1.5,
            density_floor: Some(0.01),
        }),
        Err(MatchError::NoSupportedProbes)
    ));
}

fn scene_definition(cube: &CubeFile) -> LutDefinition {
    let mut definition = definition();
    definition.lut = lut_data_from_cube(cube).expect("lut").record();
    definition
}

fn assert_import_uncertainty(document: &LookDocument) {
    let count = probes::v1().probes.len();
    let uncertainty = &document.uncertainty;
    let covariance = uncertainty
        .probe_cov_post
        .as_ref()
        .expect("post covariance");
    let sigma = uncertainty.probe_sigma_post.as_ref().expect("post sigma");
    let support = uncertainty.support.as_ref().expect("support");
    let reliable = uncertainty.probe_pre_reliable.as_ref().expect("reliable");
    let density = uncertainty.density.as_ref().expect("density");
    assert_eq!(covariance.probes, count);
    assert_eq!(covariance.values.len(), count * 9);
    assert_eq!(sigma.values.len(), count);
    assert!(sigma.values.iter().any(|value| *value > 0.0));
    assert_eq!(reliable.values.len(), count);
    assert!(
        density
            .values
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
    );
    let supported: Vec<usize> = support
        .values
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (*value > 0.5).then_some(index))
        .collect();
    assert!(!supported.is_empty());
    assert!(supported.len() < count);
    assert!(document.is_matchable());
    let probe_set = probes::v1();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probe_set,
        function: &document.look,
        uncertainty: Some(&document.uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: Some(0.01),
    })
    .expect("prepared target");
    assert_eq!(target.supported, supported);
    let unsupported = (0..count)
        .find(|index| support.values[*index] <= 0.5)
        .expect("unsupported probe");
    assert_eq!(target.weight[unsupported], 0.0);
    assert!(target.weight[supported[0]] > 0.0);

    let bytes = write_card(
        document,
        &CardMeta::new("Test", "Test"),
        &CardOptions::default(),
    )
    .expect("card");
    let card = read_card(&bytes).expect("read card");
    assert_eq!(card.fit().len(), count);
    for (record, value) in card.fit().iter().zip(&support.values) {
        assert_eq!(record.supported, *value > 0.5);
    }
}

#[test]
fn a_scene_referred_import_persists_approximation_uncertainty() {
    let cube = steep_cube();
    let imported = import_lut_look(
        identity("scene-uncertainty"),
        scene_definition(&cube),
        &cube,
        None,
        LutLookOptions::default(),
    )
    .expect("import");
    let decoded =
        LookDocument::decode(&imported.document.encode().expect("encode")).expect("decode");
    assert_import_uncertainty(&decoded);
}

#[test]
fn a_neutral_base_import_persists_approximation_uncertainty() {
    let cube = steep_cube();
    let imported = import_lut_look(
        identity("neutral-uncertainty"),
        synthetic_definition(&cube),
        &cube,
        None,
        LutLookOptions::default(),
    )
    .expect("import");
    let decoded =
        LookDocument::decode(&imported.document.encode().expect("encode")).expect("decode");
    assert_import_uncertainty(&decoded);
}
