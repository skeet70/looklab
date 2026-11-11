// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, CameraRendering, DeclaredEncoding, DynamicRange, Gamut,
    LicenseRecord, LogTransfer, LookVersion, LutData, LutDefinition, LutEmulation, LutRecord,
    MatchRefusal, OutputEncoding, Slug,
};
use looklab_model::{
    Look, LookDocument, LookIdentity, LookKind, ProbeScalars, UncertaintyData, WbPolicy,
};

fn identity(slug: &str) -> LookIdentity {
    LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive).expect("identity")
}

fn lut() -> LutData {
    LutData::new(
        2,
        [0.0; 3],
        [1.0; 3],
        (0..24).map(|value| value as f64 / 23.0).collect(),
    )
    .expect("lut")
}

fn license() -> LicenseRecord {
    LicenseRecord {
        own_work: false,
        source_license: Some("CC0-1.0".to_string()),
        source_url: None,
        attribution: "a donor".to_string(),
    }
}

fn scene_definition() -> LutDefinition {
    LutDefinition {
        name: "Scene Look".to_string(),
        license: license(),
        encoding: DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        },
        declared_base: None,
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: 2,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
        },
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    }
}

#[test]
fn lut_defined_round_trip_is_exact() {
    let document = LookDocument::from_lut_definition(
        identity("scene-look"),
        Look::colorimetric(),
        scene_definition(),
        lut(),
    )
    .expect("document");
    let bytes = document.encode().expect("encode");
    let decoded = LookDocument::decode(&bytes).expect("decode");
    assert_eq!(decoded.lut, Some(lut()));
    assert!(decoded.is_matchable());
    assert_eq!(decoded.file.provenance.base, None);
    assert_eq!(decoded.encode().expect("re-encode"), bytes);
}

#[test]
fn display_base_is_recorded_and_pending_is_not_matchable() {
    for (status, refusal) in [
        (BaseStatus::Pinned, None),
        (
            BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation,
            },
            Some(MatchRefusal::DisplayBaseUnpinned),
        ),
    ] {
        let matchable = refusal.is_none();
        let mut definition = scene_definition();
        definition.encoding = DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status,
        };
        let document = LookDocument::from_lut_definition(
            identity("display-look"),
            Look::colorimetric(),
            definition,
            lut(),
        )
        .expect("document");
        assert_eq!(document.is_matchable(), matchable);
        assert_eq!(document.match_refusal(), refusal);
        assert_eq!(
            document.file.provenance.base,
            Some(BaseReference::neutral_baseline())
        );
    }
}

#[test]
fn a_native_look_without_admitted_probes_is_unsupported() {
    let probes = looklab_model::probes::v1();
    let document = LookDocument::from_look(
        LookIdentity::new(
            "native",
            "1.0.0",
            LookKind::NativeFamily,
            WbPolicy::Adaptive,
        )
        .expect("identity"),
        Look::colorimetric(),
    )
    .expect("document")
    .with_uncertainty(UncertaintyData {
        support: Some(ProbeScalars::new(vec![0.0; probes.probes.len()]).expect("support")),
        ..UncertaintyData::default()
    })
    .expect("uncertainty");
    assert_eq!(document.match_refusal(), Some(MatchRefusal::Unsupported));
    assert!(!document.is_matchable());
}

#[test]
fn lut_data_is_bounded_and_finite() {
    assert!(LutData::new(2, [0.0; 3], [1.0; 3], vec![0.0; 23]).is_err());
    assert!(LutData::new(2, [0.0; 3], [1.0; 3], vec![f64::NAN; 24]).is_err());
    assert!(LutData::new(2, [1.0; 3], [0.0; 3], vec![0.0; 24]).is_err());
    assert!(
        LutData::new(
            looklab_model::MAX_LUT_SIDE + 1,
            [0.0; 3],
            [1.0; 3],
            vec![0.0; 24]
        )
        .is_err()
    );
}

#[test]
fn a_native_look_cannot_carry_a_definition() {
    let mut file = looklab_model::LookFile::from_look(
        LookIdentity::new(
            "native",
            "1.0.0",
            LookKind::NativeFamily,
            WbPolicy::Adaptive,
        )
        .expect("identity"),
        &Look::colorimetric(),
    )
    .expect("file");
    file.definition = Some(scene_definition());
    assert!(LookDocument::new(file, Look::colorimetric()).is_err());
}

#[test]
fn a_lut_defined_look_needs_its_stored_lut() {
    let mut file = looklab_model::LookFile::from_look(identity("missing"), &Look::colorimetric())
        .expect("file");
    file.definition = Some(scene_definition());
    assert!(LookDocument::new(file, Look::colorimetric()).is_err());
}

#[test]
fn family_base_is_identified_but_not_matchable() {
    let base = BaseReference::FamilyRendering {
        maker: "fujifilm".to_string(),
        rendering: "Eterna".to_string(),
        dynamic_range: Some(DynamicRange::Dr200),
        notes: "intended base; not applied".to_string(),
    };
    assert_eq!(
        base.resolve_status(None),
        BaseStatus::BaseIdentifiedUnmeasured
    );
    let mut definition = scene_definition();
    definition.encoding = DeclaredEncoding::DisplayReferred {
        base,
        status: BaseStatus::BaseIdentifiedUnmeasured,
    };
    let document = LookDocument::from_lut_definition(
        identity("family-look"),
        Look::colorimetric(),
        definition,
        lut(),
    )
    .expect("document");
    assert!(!document.is_matchable());
}

#[test]
fn x_e3_base_resolution_rejects_eterna_and_keeps_provia() {
    let schema = looklab_schema::validate_control_schema(include_str!(
        "../../../data/bodies/fujifilm-x-e3.yaml"
    ))
    .expect("x-e3 schema");
    let base = |sim: Option<&str>, dr| BaseReference::BodyRealization {
        body: looklab_schema::BodyId::new("fujifilm/x-e3").expect("body"),
        renderer: looklab_model::Renderer::XRawStudio,
        film_simulation: sim.map(|value| looklab_schema::SimKey::new(value).expect("sim")),
        dynamic_range: Some(dr),
        notes: String::new(),
    };
    assert_eq!(
        base(Some("eterna"), looklab_model::DynamicRange::Dr200).resolve_status(Some(&schema)),
        BaseStatus::BaseInconsistent {
            field: BaseField::FilmSimulation
        }
    );
    assert_eq!(
        base(None, looklab_model::DynamicRange::Dr200).resolve_status(Some(&schema)),
        BaseStatus::BaseIncomplete {
            missing: BaseField::FilmSimulation
        }
    );
    assert_eq!(
        base(Some("provia"), looklab_model::DynamicRange::Dr200).resolve_status(Some(&schema)),
        BaseStatus::BaseIdentifiedUnmeasured
    );
}

#[test]
fn proposal_bases_preserve_unmeasured_and_incomplete_states() {
    let family = looklab_schema::LutBase::FamilyRendering {
        maker: looklab_schema::MakerName::new("Fujifilm").expect("maker"),
        rendering: looklab_schema::RenderingName::new("Classic Chrome").expect("rendering"),
        dynamic_range: Some(looklab_schema::LutDynamicRange::Dr400),
        notes: "intended".to_owned(),
    };
    let reference = BaseReference::try_from(&family).expect("family base");
    assert_eq!(
        reference.resolve_status(None),
        BaseStatus::BaseIdentifiedUnmeasured
    );

    let body = looklab_schema::LutBase::BodyRealization {
        body: looklab_schema::BodyId::new("fujifilm/x-e3").expect("body"),
        renderer: looklab_schema::LutRenderer::XRawStudio,
        film_simulation: None,
        dynamic_range: None,
        notes: String::new(),
    };
    let reference = BaseReference::try_from(&body).expect("body base");
    let schema = looklab_schema::validate_control_schema(include_str!(
        "../../../data/bodies/fujifilm-x-e3.yaml"
    ))
    .expect("x-e3 schema");
    assert_eq!(
        reference.resolve_status(Some(&schema)),
        BaseStatus::BaseIncomplete {
            missing: BaseField::FilmSimulation
        }
    );
}

#[test]
fn native_rendering_round_trips_and_lut_defined_looks_cannot_declare_one() {
    let rendering = CameraRendering {
        maker: "Fujifilm".to_owned(),
        name: "Classic Chrome".to_owned(),
    };
    let native = LookIdentity::new(
        "classic-chrome",
        "1.0.0",
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let mut document = LookDocument::from_look(native, Look::colorimetric()).expect("document");
    document.file.rendering = Some(rendering.clone());
    let bytes = document.encode().expect("encode");
    assert_eq!(
        LookDocument::decode(&bytes).expect("decode").file.rendering,
        Some(rendering.clone())
    );

    let mut lut_document = LookDocument::from_lut_definition(
        identity("scene-look"),
        Look::colorimetric(),
        scene_definition(),
        lut(),
    )
    .expect("document");
    lut_document.file.rendering = Some(rendering);
    assert!(lut_document.encode().is_err());
}

#[test]
fn camera_rendering_supersession_matches_normalized_labels() {
    let emulation = LutEmulation::CameraRendering {
        maker: "ricoh".to_string(),
        rendering: "Cinema (Green)".to_string(),
    };
    assert!(looklab_model::emulation_matches_camera_rendering(
        &emulation,
        " Ricoh ",
        "cinema (green)"
    ));
    assert!(!looklab_model::emulation_matches_camera_rendering(
        &emulation, "leica", "Classic"
    ));
    assert!(looklab_model::emulation_matches_camera_rendering(
        &emulation,
        "RICOH",
        "cinema-green"
    ));
    assert!(!looklab_model::emulation_matches_camera_rendering(
        &LutEmulation::FilmStock {
            name: "CineStill 800T".to_string()
        },
        "ricoh",
        "Cinema (Green)"
    ));
}

#[test]
fn declared_base_records_an_approximate_computation_base() {
    let computation = BaseReference::Look {
        slug: Slug::new("native-rendering").expect("slug"),
        version: LookVersion::new("2.0.0").expect("version"),
    };
    let declared = BaseReference::FamilyRendering {
        maker: "Fujifilm".to_string(),
        rendering: "Classic Chrome".to_string(),
        dynamic_range: None,
        notes: String::new(),
    };
    let mut definition = scene_definition();
    definition.encoding = DeclaredEncoding::DisplayReferred {
        base: computation.clone(),
        status: BaseStatus::Pinned,
    };
    definition.declared_base = Some(declared);
    definition.validate().expect("approximate computation base");

    definition.declared_base = Some(computation);
    assert!(definition.validate().is_err());

    let mut scene = scene_definition();
    scene.declared_base = Some(BaseReference::neutral_baseline());
    assert!(scene.validate().is_err());
}
