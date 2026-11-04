// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{has_makernotes, pairing_data, parse_tiff, software_matches_firmware};

#[test]
fn detects_makernote_presence() {
    let with_makernote = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    assert!(has_makernotes(&with_makernote));

    let without_makernote = parse_tiff(&support::build_tiff(
        &[(0x010f, support::Value::Ascii("FUJIFILM".into()))],
        &[(0x829a, support::Value::Rational(1, 125))],
        None,
    ))
    .expect("synthetic TIFF");
    assert!(!has_makernotes(&without_makernote));
}

#[test]
fn matches_software_against_firmware_versions() {
    assert!(software_matches_firmware(
        Some("Digital Camera X-T5 Ver1.01"),
        Some("1.01")
    ));
    assert!(software_matches_firmware(
        Some("Digital Camera X-T5 Ver1.01"),
        Some("0101")
    ));
    assert!(software_matches_firmware(Some("Ver 2.10"), Some("2.10")));
    assert!(!software_matches_firmware(
        Some("Digital Camera X-T5 Ver1.01"),
        Some("2.00")
    ));
    assert!(!software_matches_firmware(None, Some("1.01")));
    assert!(!software_matches_firmware(Some("Ver1.01"), None));
}

#[test]
fn body_serial_prefers_exif_then_internal() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    assert_eq!(
        parsed.body_serial().expect("EXIF serial").envelope_ascii(),
        &b"SERIAL123"[..]
    );

    let makernote =
        support::build_makernote(&[(0x0010, support::FujiValue::Ascii("INTERNAL".into()))]);
    let tiff = support::build_tiff(&[], &[(0x927c, support::Value::Undefined(makernote))], None);
    let parsed = parse_tiff(&tiff).expect("synthetic TIFF");
    assert_eq!(
        parsed
            .body_serial()
            .expect("makernote serial")
            .envelope_ascii(),
        &b"INTERNAL"[..]
    );
}

#[test]
fn pairing_data_carries_capture_time_and_image_number() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("synthetic TIFF");
    let pairing = pairing_data(&parsed);

    let capture = pairing.capture_time.expect("capture time");
    assert_eq!((capture.year, capture.month, capture.day), (2024, 5, 17));
    assert_eq!(capture.subsec.expect("subsecond").as_str(), "123");
    assert_eq!(pairing.image_number.expect("image number").value(), 1234);
}
