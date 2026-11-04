// SPDX-License-Identifier: AGPL-3.0-or-later
mod support;

use looklab_meta::{MetaError, Orientation, parse, parse_jpeg, parse_raf, parse_tiff};

#[test]
fn parses_jpeg_app1_exif() {
    let jpeg = support::build_jpeg(&support::full_exif_tiff());
    let parsed = parse_jpeg(&jpeg).expect("JPEG has an EXIF APP1 segment");
    let exif = &parsed.exif;

    assert_eq!(exif.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(exif.model.as_deref(), Some("X-T5"));
    assert_eq!(
        exif.software.as_deref(),
        Some("Digital Camera X-T5 Ver1.01")
    );
    assert_eq!(exif.lens_model.as_deref(), Some("XF35mmF1.4 R"));
    assert_eq!(exif.orientation, Some(Orientation::Rotate90Clockwise));
    assert_eq!(exif.focal_length.expect("focal length").millimetres(), 35.0);
    assert_eq!(exif.iso.expect("ISO").value(), 400);
    assert_eq!(
        exif.exposure_time.expect("exposure time").seconds(),
        1.0 / 250.0
    );
    assert_eq!(exif.f_number.expect("f-number").f_number(), 2.8);
    assert!((exif.exposure_compensation.expect("EC").ev() - (-1.0 / 3.0)).abs() < 1e-12);

    let capture = exif.capture_time.as_ref().expect("DateTimeOriginal");
    assert_eq!((capture.year, capture.month, capture.day), (2024, 5, 17));
    assert_eq!((capture.hour, capture.minute, capture.second), (10, 11, 12));
    assert_eq!(
        capture
            .subsec
            .as_ref()
            .expect("SubSecTimeOriginal")
            .as_str(),
        "123"
    );

    assert!(exif.has_maker_note);
    assert!(exif.gps_present);
    assert!(exif.image_description_present);
    assert!(exif.artist_present);
    assert!(exif.copyright_present);
    assert!(exif.user_comment_present);
    assert!(exif.owner_name_present);
    assert!(exif.lens_serial_present);

    let serial = exif.body_serial.as_ref().expect("BodySerialNumber");
    let mut envelope = Vec::new();
    serial.write_envelope_field(&mut envelope);
    assert_eq!(&envelope[..], b"\x01\x09SERIAL123");
    assert_eq!(parsed.firmware.as_deref(), Some("1.01"));

    assert_eq!(
        exif.image_unique_id
            .as_ref()
            .expect("ImageUniqueID")
            .as_str(),
        "0123456789abcdef0123456789abcdef"
    );
    assert!(parsed.fuji.is_some());
}

#[test]
fn parses_tiff_and_dng_streams() {
    let parsed = parse_tiff(&support::full_exif_tiff()).expect("bare TIFF EXIF");
    assert_eq!(parsed.exif.model.as_deref(), Some("X-T5"));
    assert_eq!(parsed.firmware, None);
    assert!(parsed.fuji.is_some());
}

#[test]
fn parses_raf_embedded_jpeg() {
    let raf = support::build_raf(&support::build_jpeg(&support::full_exif_tiff()));
    let parsed = parse_raf(&raf).expect("RAF header points at an embedded JPEG");
    assert_eq!(parsed.exif.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(parsed.exif.model.as_deref(), Some("X-T5"));
    assert_eq!(parsed.firmware.as_deref(), Some("1.01"));
    assert!(parsed.fuji.is_some());
}

#[test]
fn raf_header_firmware_wins_over_software() {
    let jpeg = support::build_jpeg(&support::full_exif_tiff());
    let raf = support::build_raf_with_firmware(&jpeg, b"0200");
    let parsed = parse_raf(&raf).expect("RAF header points at an embedded JPEG");
    assert_eq!(parsed.firmware.as_deref(), Some("2.00"));
}

#[test]
fn hostile_exif_ifd_offset_is_ignored() {
    let tiff = support::build_tiff(
        &[
            (0x010f, support::Value::Ascii("FUJIFILM".into())),
            (0x8769, support::Value::Long(u32::MAX)),
        ],
        &[],
        None,
    );
    let parsed = parse_tiff(&tiff).expect("IFD0 is still readable");
    assert_eq!(parsed.exif.make.as_deref(), Some("FUJIFILM"));
    assert!(parsed.exif.model.is_none());
}

#[test]
fn parses_big_endian_tiff() {
    let tiff = support::build_tiff_be(
        &[
            (0x010f, support::Value::Ascii("FUJIFILM".into())),
            (0x0110, support::Value::Ascii("X-T5".into())),
        ],
        &[
            (0x829a, support::Value::Rational(1, 125)),
            (0x8827, support::Value::Short(200)),
        ],
        None,
    );
    let parsed = parse_tiff(&tiff).expect("big-endian TIFF");
    assert_eq!(parsed.exif.model.as_deref(), Some("X-T5"));
    assert_eq!(parsed.exif.iso.expect("ISO").value(), 200);
    assert_eq!(
        parsed.exif.exposure_time.expect("exposure time").seconds(),
        1.0 / 125.0
    );
}

#[test]
fn sniffs_each_supported_container() {
    let tiff = support::full_exif_tiff();
    let jpeg = support::build_jpeg(&tiff);
    let raf = support::build_raf(&jpeg);

    assert!(parse(&tiff).is_ok());
    assert!(parse(&jpeg).is_ok());
    assert!(parse(&raf).is_ok());
    assert!(matches!(
        parse(b"not a container"),
        Err(MetaError::UnknownContainer)
    ));
}

#[test]
fn reports_missing_and_malformed_metadata() {
    assert!(matches!(
        parse_jpeg(&[0xff, 0xd8, 0xff, 0xd9]),
        Err(MetaError::NoExif)
    ));
    assert!(matches!(
        parse_raf(b"FUJIFILMCCD-RAW "),
        Err(MetaError::InvalidRaf)
    ));

    let mut raf = support::build_raf(&support::build_jpeg(&support::full_exif_tiff()));
    raf[0x54..0x58].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(parse_raf(&raf), Err(MetaError::InvalidRaf)));
}
