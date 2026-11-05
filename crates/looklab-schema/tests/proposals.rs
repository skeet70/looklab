// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal validation, menu-level summaries, confirmations and fixture consent.

use looklab_schema::{
    Date, Evidence, FixtureDonation, FixtureLicense, LabeledTag, LicenseDeclaration, MappingError,
    Placement, ProposalKind, ProposalSummary, Sha256Hex, ValidatedPayload, fixture_manifest_toml,
    fixture_source, frame_fixture_upload, infer_mapping, split_fixture_upload, validate_proposal,
};

const BODY: &str = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
const INTERFACE: &str = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");

fn summary_of(kind: ProposalKind, payload: &str) -> ProposalSummary {
    validate_proposal(kind, payload)
        .expect("valid payload")
        .summary()
}

#[test]
fn a_body_proposal_summarizes_in_menu_terms() {
    let summary = summary_of(ProposalKind::Body, BODY);
    assert_eq!(summary.headline, "adds Fujifilm X-T5 as a camera body");
    assert!(summary.text().contains("body id: fujifilm/x-t5"));
    assert!(summary.text().contains("mapping: provisional"));
}

#[test]
fn a_schema_change_lists_the_menu_level_control_changes() {
    let summary = summary_of(ProposalKind::SchemaChange, SCHEMA);
    assert_eq!(summary.headline, "updates the X-T5 control schema");
    assert!(
        summary
            .details
            .contains(&"Highlight tone goes -2 to +4 in half steps".to_owned()),
        "{:?}",
        summary.details
    );
    assert!(
        summary
            .details
            .contains(&"Dynamic range offers dr100, dr200, dr400".to_owned())
    );
}

#[test]
fn a_mapping_proposal_names_the_simulation_model_and_firmware() {
    let payload = "\
body: fujifilm/x-t5
epoch: 2
sim_key: reala_ace
look: reala-ace
firmware_from: \"2.00\"
makernote: { tag: FilmMode, value: 0x0700 }
";
    let summary = summary_of(ProposalKind::Mapping, payload);
    assert_eq!(summary.headline, "adds Reala Ace to X-T5 firmware 2.00+");
    assert!(
        summary
            .details
            .contains(&"makernote: FilmMode = 0x0700".to_owned())
    );
}

#[test]
fn a_mapping_with_an_inverted_firmware_range_is_rejected() {
    let payload = "\
body: fujifilm/x-t5
epoch: 2
sim_key: reala_ace
look: reala-ace
firmware_from: \"2.10\"
firmware_to: \"2.00\"
makernote: { tag: FilmMode, value: 0x0700 }
";
    let errors = validate_proposal(ProposalKind::Mapping, payload).unwrap_err();
    assert_eq!(errors[0].path, "firmware_to");
}

#[test]
fn a_lut_look_requires_a_license_declaration_and_attribution() {
    let no_license = "\
look: my-lut
name: My LUT
license: { own_work: false }
attribution: A contributor
";
    let errors = validate_proposal(ProposalKind::LutLook, no_license).unwrap_err();
    assert_eq!(errors[0].path, "license");

    let valid = "\
look: my-lut
name: My LUT
license: { own_work: true }
attribution: A contributor
";
    let summary = summary_of(ProposalKind::LutLook, valid);
    assert_eq!(summary.headline, "adds the LUT-defined look My LUT");
    assert!(summary.text().contains("license: the submitter's own work"));
    assert!(summary.text().contains("published under CC0-1.0"));

    let licensed = LicenseDeclaration {
        own_work: false,
        source_license: Some("CC-BY-4.0".to_owned()),
        source_url: None,
    };
    assert!(licensed.validate().is_empty());
    assert!(licensed.describe().contains("CC-BY-4.0"));
}

#[test]
fn an_interface_profile_summarizes_its_insertion_point() {
    let summary = summary_of(ProposalKind::InterfaceProfile, INTERFACE);
    assert_eq!(
        summary.headline,
        "adds the Example editor 3D LUT module software interface profile"
    );
    assert!(
        summary
            .text()
            .contains("accepts: cube, Hald clut, native JSON")
    );
    assert!(summary.text().contains("bodies: Model-A"));
}

#[test]
fn an_epoch_split_names_the_firmware_boundary() {
    let payload = "\
body: fujifilm/x-t5
from_epoch: 1
new_epoch: 2
split_at: \"2.00\"
";
    let summary = summary_of(ProposalKind::EpochSplit, payload);
    assert_eq!(
        summary.headline,
        "splits X-T5 into a new epoch at firmware 2.00"
    );
}

#[test]
fn a_look_split_and_a_gate_override_have_summaries() {
    let split = summary_of(
        ProposalKind::LookSplit,
        "look: classic-chrome\nnew_look: classic-chrome-mono\nname: Classic Chrome Mono\n",
    );
    assert_eq!(
        split.headline,
        "splits classic-chrome into classic-chrome-mono"
    );

    let gate = summary_of(
        ProposalKind::GateOverride,
        "look: classic-chrome\nversion: 3\njustification: new root session\n",
    );
    assert_eq!(
        gate.headline,
        "overrides the release gate for classic-chrome version 3"
    );
    assert!(gate.text().contains("new root session"));
}

#[test]
fn a_payload_over_the_size_limit_is_rejected() {
    let payload = "x".repeat(256 * 1024 + 1);
    assert!(validate_proposal(ProposalKind::Body, &payload).is_err());
}

#[test]
fn confirmations_are_limited_to_whitelisted_tags() {
    let film_mode = LabeledTag {
        tag: "FilmMode".try_into().expect("tag"),
        value: looklab_schema::MakernoteValue(0x0700),
        label: "classic_chrome".try_into().expect("label"),
    };
    looklab_schema::validate_confirmation(std::slice::from_ref(&film_mode)).expect("whitelisted");

    let gps = LabeledTag {
        tag: "GPSLatitude".try_into().expect("tag"),
        value: looklab_schema::MakernoteValue(1),
        label: "classic_chrome".try_into().expect("label"),
    };
    let errors = looklab_schema::validate_confirmation(&[gps]).unwrap_err();
    assert_eq!(errors[0].path, "tags[0].tag");

    let duplicate =
        looklab_schema::validate_confirmation(&[film_mode.clone(), film_mode]).unwrap_err();
    assert_eq!(duplicate[0].path, "tags[1].tag");
}

fn donation() -> FixtureDonation {
    FixtureDonation {
        file: "fujifilm-x-t5-2.00.raf".to_owned(),
        description: "X-T5 firmware 2.00, Reala Ace".to_owned(),
        make: "Fujifilm".to_owned(),
        model: "X-T5".to_owned(),
        sha256: Sha256Hex::parse(&"ab".repeat(32)).expect("sha256"),
        size: 24_000_000,
        source_url: Some("https://example.com/fujifilm-x-t5-2.00.raf".to_owned()),
        source: None,
        donator: Some("a_handle".to_owned()),
        donated_on: Date::parse("2026-11-04").expect("date"),
        license: FixtureLicense::Cc0_1_0,
        placement: Placement::Fetch,
    }
}

#[test]
fn a_download_only_fixture_needs_a_source() {
    let published = FixtureDonation {
        source_url: None,
        placement: Placement::Git,
        ..donation()
    };
    published
        .validate()
        .expect("a committed fixture needs no URL");
    assert!(!fixture_manifest_toml(std::slice::from_ref(&published)).contains("source_url"));

    let download_only = FixtureDonation {
        source_url: None,
        ..donation()
    };
    let errors = download_only.validate().unwrap_err();
    assert_eq!(errors[0].path, "source");
}

#[test]
fn an_uploaded_source_is_bound_to_the_file_digest() {
    let uploaded = FixtureDonation {
        source_url: None,
        source: Some(fixture_source(donation().sha256)),
        ..donation()
    };
    uploaded.validate().expect("an uploaded source is valid");
    let manifest = fixture_manifest_toml(std::slice::from_ref(&uploaded));
    assert!(manifest.contains(&format!(
        "source = \"{}\"",
        fixture_source(donation().sha256)
    )));
    assert!(!manifest.contains("source_url"));

    let mismatched = FixtureDonation {
        source: Some(fixture_source(
            Sha256Hex::parse(&"cd".repeat(32)).expect("sha256"),
        )),
        ..uploaded.clone()
    };
    assert_eq!(mismatched.validate().unwrap_err()[0].path, "source");

    let published = FixtureDonation {
        placement: Placement::Git,
        ..uploaded
    };
    assert_eq!(published.validate().unwrap_err()[0].path, "source");
}

#[test]
fn a_fixture_upload_frame_round_trips() {
    let header = br#"{"file":"a.raf","sha256":"00","size":1}"#;
    let frame = frame_fixture_upload(header, b"fixture bytes").expect("frame");
    let (parsed, bytes) = split_fixture_upload(&frame).expect("split");
    assert_eq!(parsed, header);
    assert_eq!(bytes, b"fixture bytes");
    assert!(split_fixture_upload(b"not a frame").is_err());
}

#[test]
fn a_fixture_backed_body_needs_a_matching_donation() {
    let body = looklab_schema::validate_body(BODY).expect("body");
    assert!(looklab_schema::mapping_status_supported(&body, &[]));

    let mut backed = body.clone();
    backed.mapping_status = looklab_schema::MappingStatus::FixtureBacked;
    assert!(!looklab_schema::mapping_status_supported(&backed, &[]));

    let other = FixtureDonation {
        model: "X-S10".to_owned(),
        ..donation()
    };
    assert!(!looklab_schema::mapping_status_supported(
        &backed,
        std::slice::from_ref(&other)
    ));
    assert!(looklab_schema::mapping_status_supported(
        &backed,
        std::slice::from_ref(&donation())
    ));
}

#[test]
fn a_fixture_donation_validates_and_renders_a_manifest_table() {
    let donation = donation();
    donation.validate().expect("valid donation");
    let toml = fixture_manifest_toml(std::slice::from_ref(&donation));
    assert!(toml.contains("[[fixture]]"));
    assert!(toml.contains("file = \"fujifilm-x-t5-2.00.raf\""));
    assert!(toml.contains("license = \"CC0-1.0\""));
    assert!(toml.contains("consent = \"Donated by a_handle on 2026-11-04 under CC0-1.0\""));
    assert!(toml.contains("committed = false"));

    let mut unsafe_name = donation.clone();
    unsafe_name.file = "../secret.raf".to_owned();
    assert!(unsafe_name.validate().is_err());
}

#[test]
fn fixture_placement_round_trips_as_json_and_in_the_manifest() {
    for (placement, json, rendered) in [
        (Placement::Git, serde_json::json!(true), "committed = true"),
        (
            Placement::Fetch,
            serde_json::json!(false),
            "committed = false",
        ),
        (
            Placement::Lfs,
            serde_json::json!("lfs"),
            "committed = \"lfs\"",
        ),
    ] {
        let encoded = serde_json::to_value(placement).expect("json");
        assert_eq!(encoded, json);
        let decoded: Placement = serde_json::from_value(encoded).expect("placement");
        assert_eq!(decoded, placement);

        let donation = FixtureDonation {
            placement,
            ..donation()
        };
        let value = serde_json::to_value(&donation).expect("json");
        assert_eq!(value["committed"], json);
        let parsed: FixtureDonation = serde_json::from_value(value).expect("donation");
        assert_eq!(parsed, donation);

        let manifest = fixture_manifest_toml(std::slice::from_ref(&donation));
        assert!(manifest.contains(rendered), "{manifest}");
    }
}

#[test]
fn evidence_round_trips_as_json() {
    let evidence = Evidence::Confirmation {
        author: Some("a_handle".to_owned()),
        tags: vec![LabeledTag {
            tag: "FilmMode".try_into().expect("tag"),
            value: looklab_schema::MakernoteValue(0x0700),
            label: "classic_chrome".try_into().expect("label"),
        }],
        recorded_at: "2026-11-04T12:00:00Z".to_owned(),
    };
    let json = serde_json::to_string(&evidence).expect("json");
    let parsed: Evidence = serde_json::from_str(&json).expect("evidence");
    assert_eq!(parsed, evidence);
}

#[test]
fn mapping_inference_chooses_the_separating_tag() {
    let files: Vec<looklab_schema::LabeledFile> = [
        ("classic_chrome", 0x0700u64),
        ("provia", 0x0000),
        ("acros", 0x0500),
    ]
    .into_iter()
    .map(|(label_name, value)| looklab_schema::LabeledFile {
        label: label_name.try_into().expect("label"),
        tags: [(
            "FilmMode".try_into().expect("tag"),
            looklab_schema::MakernoteValue(value),
        )]
        .into_iter()
        .collect(),
    })
    .collect();
    let inference = infer_mapping(&files).expect("inference");
    let best = inference.best().expect("consistent tag");
    assert_eq!(best.tag.as_str(), "FilmMode");
    assert_eq!(best.confidence, 1.0);

    assert_eq!(infer_mapping(&files[..1]), Err(MappingError::TooFewFiles));
}

#[test]
fn a_body_document_that_is_not_valid_is_rejected() {
    assert!(validate_proposal(ProposalKind::Body, "id: fujifilm/x-t5\n").is_err());
}

#[test]
fn validation_does_not_mutate_the_summary_for_the_same_payload() {
    let first = summary_of(ProposalKind::SchemaChange, SCHEMA);
    let second = summary_of(ProposalKind::SchemaChange, SCHEMA);
    assert_eq!(first, second);
    match validate_proposal(ProposalKind::SchemaChange, SCHEMA).expect("valid") {
        ValidatedPayload::SchemaChange(schema) => assert_eq!(schema.body.as_str(), "fujifilm/x-t5"),
        other => panic!("unexpected payload {other:?}"),
    }
}
