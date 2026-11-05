// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_export::card::{CardError, CardMeta, CardOptions, write_card};
use looklab_export::{CubeFile, Lut3d, LutLookOptions, definition_delta, import_lut_look};
use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, DeclaredEncoding, Gamut, LicenseRecord, LogTransfer,
    LutData, LutDefinition, LutRecord, OutputEncoding,
};
use looklab_model::{LookIdentity, LookKind, WbPolicy};

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
        Err(CardError::NotMatchable)
    ));

    let mut pinned = definition();
    pinned.encoding = DeclaredEncoding::DisplayReferred {
        base: BaseReference::neutral_baseline(),
        status: BaseStatus::Pinned,
    };
    let imported =
        import_lut_look(identity("pinned-look"), pinned, &cube, None, options()).expect("import");
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
