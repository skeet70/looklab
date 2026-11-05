// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{
    EffectStrength, FilmMode, RemovedField, WhiteBalanceMultipliers, parse_jpeg, parse_tiff,
    sanitize,
};

#[test]
fn sanitizes_only_documented_fields() {
    let parsed = parse_jpeg(&support::full_exif_jpeg()).expect("synthetic JPEG");
    let (sanitized, _) = sanitize(&parsed);

    assert_eq!(sanitized.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(sanitized.model.as_deref(), Some("X-T5"));
    assert_eq!(sanitized.firmware.as_deref(), Some("1.01"));
    assert_eq!(sanitized.lens_model.as_deref(), Some("XF35mmF1.4 R"));
    assert_eq!(
        sanitized.focal_length.expect("focal length").millimetres(),
        35.0
    );
    assert_eq!(sanitized.iso.expect("ISO").value(), 400);
    assert_eq!(sanitized.shutter.expect("shutter").seconds(), 1.0 / 250.0);
    assert_eq!(sanitized.aperture.expect("aperture").f_number(), 2.8);
    assert_eq!(
        sanitized.capture_month.expect("capture month").to_string(),
        "2024-05"
    );
    assert_eq!(
        sanitized.software.as_deref(),
        Some("Digital Camera X-T5 Ver1.01")
    );
    assert_eq!(sanitized.as_shot_wb, None);

    let look = sanitized.look.expect("look settings");
    assert_eq!(
        look.film_mode.expect("film mode").value,
        Some(FilmMode::Eterna)
    );
    assert_eq!(
        look.color_chrome_effect.expect("color chrome").value,
        Some(EffectStrength::Strong)
    );
}

#[test]
fn serialized_metadata_contains_no_dropped_values() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    let (sanitized, removed) = sanitize(&parsed);
    let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");

    support::assert_whitelisted_keys(&json);
    for secret in [
        "SERIAL123",
        "0123456789abcdef0123456789abcdef",
        "Test Artist",
        "Test Copyright",
        "Test image",
        "Owner",
        "LENS-SERIAL",
        "FPX20017035",
        "comment",
        "2024:05:17",
        "10:11:12",
    ] {
        assert!(!json.contains(secret), "leaked {secret:?} in {json}");
    }

    let removed_json = serde_json::to_string(&removed).expect("removed fields are JSON");
    assert!(removed_json.contains("[redacted]"));
    assert!(!removed_json.contains("SERIAL123"));
    assert!(!removed_json.contains("0123456789abcdef0123456789abcdef"));
    assert!(!removed_json.contains("Test Artist"));
}

#[test]
fn render_tag_leaves_only_parsed_fields() {
    const TOKEN: &str = "looklab-card/1 fw=acme-sim/2.0.1 look=provia@1.0.0 cid=0123456789abcdef";
    let mut payload = b"ASCII\0\0\0".to_vec();
    payload.extend_from_slice(TOKEN.as_bytes());
    let tiff = support::build_tiff(&[], &[(0x9286, support::Value::Undefined(payload))], None);
    let parsed = parse_tiff(&tiff).expect("synthetic TIFF");
    let (sanitized, removed) = sanitize(&parsed);

    let tag = sanitized.render_tag.as_ref().expect("render tag");
    assert_eq!(tag.firmware_id.as_str(), "acme-sim");
    assert_eq!(tag.firmware_version.as_str(), "2.0.1");
    assert_eq!(tag.look_slug.as_str(), "provia");
    assert_eq!(tag.look_version.as_str(), "1.0.0");
    assert_eq!(tag.cid_prefix_hex(), "0123456789abcdef");

    let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");
    support::assert_whitelisted_keys(&json);
    assert!(!json.contains("looklab-card/1"), "raw tag leaked in {json}");
    assert!(removed.contains(RemovedField::UserComment));
}

#[test]
fn malformed_render_tag_leaves_only_the_flag() {
    const TOKEN: &str = "looklab-card/1 fw=acme-sim/2.0.1 look=provia@1.0.0 cid=NOT-A-CID";
    let mut payload = b"ASCII\0\0\0".to_vec();
    payload.extend_from_slice(TOKEN.as_bytes());
    let tiff = support::build_tiff(&[], &[(0x9286, support::Value::Undefined(payload))], None);
    let parsed = parse_tiff(&tiff).expect("synthetic TIFF");
    let (sanitized, removed) = sanitize(&parsed);

    assert!(sanitized.render_tag.is_none());
    assert!(sanitized.render_tag_malformed);
    let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");
    support::assert_whitelisted_keys(&json);
    assert!(!json.contains(TOKEN), "raw tag leaked in {json}");
    assert!(removed.contains(RemovedField::UserComment));
}

#[test]
fn removed_fields_name_every_dropped_picture_identifier() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    let (_, removed) = sanitize(&parsed);

    for field in [
        RemovedField::GpsLocation,
        RemovedField::ImageDescription,
        RemovedField::Artist,
        RemovedField::Copyright,
        RemovedField::UserComment,
        RemovedField::CameraOwnerName,
        RemovedField::LensSerialNumber,
        RemovedField::ImageUniqueId,
        RemovedField::BodySerialNumber,
        RemovedField::InternalSerialNumber,
        RemovedField::MakerNote,
    ] {
        assert!(removed.contains(field), "missing {field:?}");
    }
    assert_eq!(removed.len(), 11);
    assert_eq!(
        removed.names().collect::<Vec<_>>(),
        [
            "GPS location",
            "Image description",
            "Artist",
            "Copyright",
            "User comment",
            "Camera owner name",
            "Lens serial number",
            "Image unique id",
            "Body serial number",
            "Internal serial number",
            "MakerNote",
        ]
    );
}

#[test]
fn caller_sets_as_shot_white_balance() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    let (sanitized, _) = sanitize(&parsed);
    let sanitized = sanitized.with_as_shot_wb(WhiteBalanceMultipliers {
        r: 2.0,
        g: 1.0,
        b: 1.5,
    });

    let value: serde_json::Value =
        serde_json::from_str(&serde_json::to_string(&sanitized).expect("JSON")).expect("JSON");
    assert_eq!(value["as_shot_wb"]["r"], 2.0);
    assert_eq!(value["as_shot_wb"]["g"], 1.0);
    assert_eq!(value["as_shot_wb"]["b"], 1.5);
}

#[test]
fn absent_fields_stay_absent() {
    let tiff = support::build_tiff(
        &[(0x010f, support::Value::Ascii("FUJIFILM".into()))],
        &[(0x829a, support::Value::Rational(1, 60))],
        None,
    );
    let parsed = parse_tiff(&tiff).expect("synthetic TIFF");
    let (sanitized, removed) = sanitize(&parsed);

    assert_eq!(sanitized.make.as_deref(), Some("FUJIFILM"));
    assert!(sanitized.model.is_none());
    assert!(sanitized.firmware.is_none());
    assert!(sanitized.capture_month.is_none());
    assert!(sanitized.look.is_none());
    assert!(removed.is_empty());

    let json = serde_json::to_string(&sanitized).expect("JSON");
    assert!(!json.contains("look"));
    assert!(!json.contains("firmware"));
    support::assert_whitelisted_keys(&json);
}
