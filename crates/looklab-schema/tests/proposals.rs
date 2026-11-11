// SPDX-License-Identifier: AGPL-3.0-or-later
//! Proposal validation, menu-level summaries, confirmations and fixture consent.

use looklab_schema::{
    ControlChange, ControlEdit, ControlKey, Date, Evidence, FixtureDonation, FixtureLicense,
    LUT_UPLOAD_MAGIC, LabeledTag, LicenseDeclaration, MAX_LUT_BYTES, MappingError, Placement,
    ProposalKind, Rational, Sha256Hex, apply_control_changes, body_id_from_name,
    fixture_manifest_toml, fixture_source, frame_fixture_upload, frame_lut_proposal, infer_mapping,
    look_id_from_name, sim_key_from_name, split_fixture_upload, split_lut_proposal,
    validate_control_schema, validate_proposal,
};

const BODY: &str = include_str!("../../../data/bodies/fujifilm-x-t5.body.yaml");
const SCHEMA: &str = include_str!("../../../data/bodies/fujifilm-x-t5.yaml");
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
lut: { file: my-lut.cube, sha256: 0000000000000000000000000000000000000000000000000000000000000000, size: 1024 }
encoding: flog2_fgamut
";
    let errors = validate_proposal(ProposalKind::LutLook, no_license).unwrap_err();
    assert_eq!(errors[0].path, "license");

    let valid = "\
look: my-lut
name: My LUT
license: { own_work: true }
attribution: A contributor
lut: { file: my-lut.cube, sha256: 0000000000000000000000000000000000000000000000000000000000000000, size: 1024 }
encoding: flog2_fgamut
";
    validate_proposal(ProposalKind::LutLook, valid).expect("licensed LUT");

    let licensed = LicenseDeclaration {
        own_work: false,
        source_license: Some("CC-BY-4.0".to_owned()),
        source_url: None,
    };
    assert!(licensed.validate().is_empty());
}

#[test]
fn a_malformed_display_lut_base_is_rejected() {
    let digest = "00".repeat(32);
    let payload = |base: &str| {
        format!(
            "look: my-lut\nname: My LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut: {{ file: my-lut.cube, sha256: {digest}, size: 1024 }}\n\
             encoding: !display\n  base:\n{base}"
        )
    };
    validate_proposal(
        ProposalKind::LutLook,
        &payload(
            "      kind: family_rendering\n      maker: Fujifilm\n      rendering: Classic Chrome\n",
        ),
    )
    .expect("valid family declaration");
    assert!(
        validate_proposal(
            ProposalKind::LutLook,
            &payload(
                "      kind: family_rendering\n      maker: \" \"\n      rendering: Classic Chrome\n",
            ),
        )
        .is_err()
    );

    assert!(
        validate_proposal(
            ProposalKind::LutLook,
            &payload(&format!(
                "      kind: family_rendering\n      maker: {}\n      rendering: Classic Chrome\n",
                "m".repeat(201)
            )),
        )
        .is_err()
    );

    let bad_look = validate_proposal(
        ProposalKind::LutLook,
        &payload("      kind: look\n      look: BadLook\n      version: 1.0.0\n"),
    )
    .unwrap_err();
    assert_eq!(bad_look[0].path, "encoding.base.look");

    let long_notes = validate_proposal(
        ProposalKind::LutLook,
        &payload(&format!(
            "      kind: family_rendering\n      maker: Fujifilm\n      rendering: Classic Chrome\n      notes: {}\n",
            "n".repeat(501)
        )),
    )
    .unwrap_err();
    assert_eq!(long_notes[0].path, "encoding.base.notes");
}

#[test]
fn a_display_lut_base_rejects_fields_from_another_kind() {
    let digest = "00".repeat(32);
    let payload = |base: &str| {
        format!(
            "look: my-lut\nname: My LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut: {{ file: my-lut.cube, sha256: {digest}, size: 1024 }}\n\
             encoding: !display\n  base:\n{base}"
        )
    };
    for base in [
        "      kind: look\n      look: my-base\n      version: 1.0.0\n      body: fujifilm/x-t5\n",
        "      kind: body_realization\n      body: fujifilm/x-t5\n      renderer: x_raw_studio\n      maker: Fujifilm\n",
        "      kind: family_rendering\n      maker: Fujifilm\n      rendering: Classic Chrome\n      body: fujifilm/x-t5\n",
        "      kind: family_rendering\n      maker: Fujifilm\n      rendering: Classic Chrome\n      unknown: value\n",
    ] {
        assert!(validate_proposal(ProposalKind::LutLook, &payload(base)).is_err());
    }

    let status = format!(
        "look: my-lut\nname: My LUT\nlicense: {{ own_work: true }}\n\
         attribution: A contributor\nlut: {{ file: my-lut.cube, sha256: {digest}, size: 1024 }}\n\
         encoding: !display\n  base:\n      kind: look\n      look: my-base\n      version: 1.0.0\n  status:\n    kind: pinned\n"
    );
    assert!(validate_proposal(ProposalKind::LutLook, &status).is_err());
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

#[derive(serde::Deserialize)]
struct ParsedManifest {
    #[serde(default)]
    fixture: Vec<ParsedFixture>,
}

#[derive(serde::Deserialize)]
struct ParsedFixture {
    file: String,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    consent: Option<String>,
    #[serde(default)]
    source_url: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default, rename = "committed")]
    placement: Placement,
}

fn parse_manifest(text: &str) -> ParsedFixture {
    let mut manifest: ParsedManifest = toml::from_str(text).expect("manifest parses");
    assert_eq!(manifest.fixture.len(), 1, "{text}");
    manifest.fixture.remove(0)
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
    let manifest = fixture_manifest_toml(std::slice::from_ref(&published));
    assert!(parse_manifest(&manifest).source_url.is_none());

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
    let parsed = parse_manifest(&fixture_manifest_toml(std::slice::from_ref(&uploaded)));
    assert_eq!(parsed.source, Some(fixture_source(donation().sha256)));
    assert!(parsed.source_url.is_none());

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
    let parsed = parse_manifest(&fixture_manifest_toml(std::slice::from_ref(&donation)));
    assert_eq!(parsed.file, "fujifilm-x-t5-2.00.raf");
    assert_eq!(parsed.license.as_deref(), Some("CC0-1.0"));
    assert_eq!(
        parsed.consent.as_deref(),
        Some("Donated by a_handle on 2026-11-04 under CC0-1.0")
    );
    assert_eq!(parsed.placement, Placement::Fetch);

    let mut unsafe_name = donation.clone();
    unsafe_name.file = "../secret.raf".to_owned();
    assert!(unsafe_name.validate().is_err());
}

#[test]
fn fixture_placement_round_trips_as_json_and_in_the_manifest() {
    for (placement, json) in [
        (Placement::Git, serde_json::json!(true)),
        (Placement::Fetch, serde_json::json!(false)),
        (Placement::Lfs, serde_json::json!("lfs")),
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

        let manifest = parse_manifest(&fixture_manifest_toml(std::slice::from_ref(&donation)));
        assert_eq!(manifest.file, donation.file);
        assert_eq!(manifest.placement, placement);
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
fn a_custom_look_cannot_take_the_reserved_baseline_or_exceed_the_slug_limit() {
    let payload = |look: &str| {
        format!(
            "look: {look}\nname: My LUT\nlicense: {{ own_work: true }}\n\
             attribution: A contributor\nlut:\n  file: my-lut.cube\n  sha256: {}\n  \
             size: 4096\nencoding: flog2_fgamut\n",
            "ab".repeat(32)
        )
    };
    let reserved = validate_proposal(
        ProposalKind::LutLook,
        &payload(looklab_schema::RESERVED_LUT_LOOK_SLUG),
    )
    .unwrap_err();
    assert_eq!(reserved[0].path, "look");
    let too_long = validate_proposal(ProposalKind::LutLook, &payload(&"a".repeat(65))).unwrap_err();
    assert_eq!(too_long[0].path, "look");
}

#[test]
fn a_lut_frame_carries_the_header_and_file_and_rejects_other_magics() {
    let header = br#"{"kind":"lut_look"}"#;
    let frame = frame_lut_proposal(header, b"cube bytes").expect("frame");
    let (parsed, bytes) = split_lut_proposal(&frame).expect("split");
    assert_eq!(parsed, header);
    assert_eq!(bytes, b"cube bytes");
    let fixture = frame_fixture_upload(header, b"cube bytes").expect("fixture frame");
    assert!(split_lut_proposal(&fixture).is_err());
    let start = LUT_UPLOAD_MAGIC.len() + 4;
    assert!(split_lut_proposal(&frame[..start + 1]).is_err());
}

#[test]
fn a_lut_file_outside_the_upload_bounds_is_rejected() {
    let payload = |file: &str, size: u64| {
        format!(
            "look: my-lut\nname: My LUT\nlicense: {{ own_work: true }}\nattribution: A contributor\n\
             lut:\n  file: {file}\n  sha256: {}\n  size: {size}\nencoding: flog2_fgamut\n",
            "ab".repeat(32)
        )
    };
    let oversized = validate_proposal(
        ProposalKind::LutLook,
        &payload("my-lut.cube", MAX_LUT_BYTES + 1),
    )
    .unwrap_err();
    assert_eq!(oversized[0].path, "size");
    let empty = validate_proposal(ProposalKind::LutLook, &payload("my-lut.cube", 0)).unwrap_err();
    assert_eq!(empty[0].path, "size");
    let wrong_extension =
        validate_proposal(ProposalKind::LutLook, &payload("my-lut.txt", 4096)).unwrap_err();
    assert_eq!(wrong_extension[0].path, "file");
}

#[test]
fn a_control_change_sets_only_the_requested_field() {
    let key = ControlKey::new("highlight_tone").expect("control key");
    let step = Rational::new(1, 4).expect("step");
    let updated = apply_control_changes(
        SCHEMA,
        &[(
            1,
            ControlChange::Set {
                key,
                edits: vec![ControlEdit::Step(step)],
            },
        )],
    )
    .expect("updated schema");
    let schema = validate_control_schema(&updated).expect("valid schema");
    let control = schema.epochs[0]
        .controls
        .iter()
        .find(|control| control.key.as_str() == "highlight_tone")
        .expect("control");
    assert_eq!(control.step, Some(step));
    assert_eq!(
        control.range.as_ref().expect("range").max,
        Rational::new(4, 1).expect("max")
    );
    assert!(control.look_affecting);
}

#[test]
fn a_control_change_rejects_a_control_the_epoch_does_not_declare() {
    let key = ControlKey::new("not_a_control").expect("control key");
    let change = ControlChange::Set {
        key,
        edits: vec![ControlEdit::LookAffecting(true)],
    };
    assert!(apply_control_changes(SCHEMA, &[(1, change)]).is_err());
}

#[test]
fn names_reduce_to_stable_ids_and_an_empty_name_is_rejected() {
    assert_eq!(
        look_id_from_name("My LUT!").expect("look").as_str(),
        "my-lut"
    );
    assert_eq!(
        body_id_from_name("Fujifilm", "X-T5")
            .expect("body")
            .as_str(),
        "fujifilm/x-t5"
    );
    assert_eq!(
        sim_key_from_name("Reala Ace").expect("sim").as_str(),
        "reala_ace"
    );
    assert!(look_id_from_name("---").is_err());
}
