// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_meta::{
    CaptureTime, ExifData, FujiMakernote, ImageNumber, LookSettings, ParsedMetadata, ParsedTag,
    SubSec,
};
use looklab_prep::{JpegRole, LocalImageKey, PairingFile, UnpairedReason, pair};

fn key(name: &str) -> LocalImageKey {
    LocalImageKey::new(name).expect("valid file name")
}

fn capture(second: u8, subsec: Option<&str>) -> CaptureTime {
    CaptureTime::new(2026, 5, 1, 10, 30, second, subsec.and_then(SubSec::new)).expect("valid time")
}

fn metadata(
    make: &str,
    model: &str,
    time: Option<CaptureTime>,
    image_number: Option<u16>,
    film: Option<looklab_meta::FilmMode>,
) -> ParsedMetadata {
    let look = LookSettings {
        film_mode: film.map(|value| ParsedTag {
            tag: looklab_meta::FujiTag::FilmMode,
            raw: 0x0100,
            value: Some(value),
        }),
        ..LookSettings::default()
    };
    let fuji = film.is_some().then(|| FujiMakernote {
        maker_note_version: None,
        internal_serial: None,
        look,
        image_generation: None,
        image_count: image_number.map(ImageNumber::new),
        sequence_number: None,
    });
    ParsedMetadata {
        exif: ExifData {
            make: Some(make.to_owned()),
            model: Some(model.to_owned()),
            software: None,
            lens_model: None,
            orientation: None,
            focal_length: None,
            iso: None,
            exposure_time: None,
            f_number: None,
            exposure_compensation: None,
            capture_time: time,
            body_serial: None,
            image_unique_id: None,
            has_maker_note: fuji.is_some(),
            gps_present: false,
            image_description_present: false,
            artist_present: false,
            copyright_present: false,
            user_comment_present: false,
            owner_name_present: false,
            lens_serial_present: false,
        },
        fuji,
        firmware: None,
    }
}

fn file(name: &str, metadata: &ParsedMetadata) -> PairingFile {
    PairingFile::new(key(name), metadata)
}

#[test]
fn pairs_by_base_name_regardless_of_input_order() {
    let raw = file(
        "DSCF0100.RAF",
        &metadata("FUJIFILM", "X-S10", Some(capture(1, None)), None, None),
    );
    let jpeg = file(
        "DSCF0100.JPG",
        &metadata("FUJIFILM", "X-S10", Some(capture(1, None)), None, None),
    );
    let forward = pair(&[raw.clone(), jpeg.clone()]);
    let backward = pair(&[jpeg, raw]);

    assert_eq!(forward, backward);
    assert_eq!(forward.pairs.len(), 1);
    assert_eq!(forward.pairs[0].raw, key("DSCF0100.RAF"));
    assert_eq!(forward.pairs[0].jpegs.len(), 1);
    assert_eq!(forward.pairs[0].jpegs[0].key, key("DSCF0100.JPG"));
    assert!(forward.unpaired.is_empty());
}

#[test]
fn pairs_by_capture_time_when_names_differ() {
    let raw = file(
        "DSCF0101.RAF",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(2, Some("480"))),
            None,
            None,
        ),
    );
    let jpeg = file(
        "RENDER.DSCF0101-1.jpg",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(2, Some("480"))),
            None,
            None,
        ),
    );
    let outcome = pair(&[raw, jpeg]);
    assert_eq!(outcome.pairs.len(), 1);
    assert_eq!(outcome.pairs[0].jpegs[0].role, JpegRole::Rerender);
}

#[test]
fn differing_bodies_leave_both_files_unpaired() {
    let raw = file(
        "IMG_0001.CR2",
        &metadata("Canon", "EOS 60D", Some(capture(3, None)), None, None),
    );
    let jpeg = file(
        "IMG_0001.JPG",
        &metadata("FUJIFILM", "X-S10", Some(capture(3, None)), None, None),
    );
    let outcome = pair(&[raw, jpeg]);
    assert!(outcome.pairs.is_empty());
    assert_eq!(outcome.unpaired.len(), 2);
    for entry in &outcome.unpaired {
        assert_eq!(entry.reason, UnpairedReason::BodyMismatch);
    }
}

#[test]
fn one_jpeg_at_two_raws_capture_time_is_ambiguous() {
    let raw_a = file(
        "A.RAF",
        &metadata("FUJIFILM", "X-S10", Some(capture(4, None)), None, None),
    );
    let raw_b = file(
        "B.RAF",
        &metadata("FUJIFILM", "X-S10", Some(capture(4, None)), None, None),
    );
    let jpeg = file(
        "C.JPG",
        &metadata("FUJIFILM", "X-S10", Some(capture(4, None)), None, None),
    );
    let outcome = pair(&[raw_a, raw_b, jpeg]);
    assert!(outcome.pairs.is_empty());
    assert_eq!(outcome.unpaired.len(), 3);
    for entry in &outcome.unpaired {
        assert_eq!(entry.reason, UnpairedReason::AmbiguousCounterpart);
    }
}

#[test]
fn classifies_in_camera_renders_and_duplicates() {
    let raw = file(
        "DSCF0102.RAF",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(5, None)),
            Some(12),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let first = file(
        "DSCF0102.JPG",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(5, None)),
            Some(12),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let second = file(
        "DSCF0102-1.JPG",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(5, None)),
            Some(12),
            Some(looklab_meta::FilmMode::ClassicChrome),
        ),
    );
    let duplicate = file(
        "DSCF0102-2.JPG",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(5, None)),
            Some(12),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let outcome = pair(&[raw, first, second, duplicate]);
    let pair = &outcome.pairs[0];
    assert_eq!(
        pair.rerender_members().collect::<Vec<_>>(),
        vec![&key("DSCF0102-1.JPG"), &key("DSCF0102-2.JPG")]
    );
    assert_eq!(
        pair.jpegs[2].role,
        JpegRole::Duplicate {
            of: key("DSCF0102-2.JPG")
        }
    );
    assert_eq!(outcome.rerender_groups().len(), 1);
}

#[test]
fn same_name_with_another_capture_time_is_a_separate_shot() {
    let raw = file(
        "DSCF0103.RAF",
        &metadata("FUJIFILM", "X-S10", Some(capture(6, None)), None, None),
    );
    let jpeg = file(
        "DSCF0103.JPG",
        &metadata("FUJIFILM", "X-S10", Some(capture(7, None)), None, None),
    );
    let outcome = pair(&[raw, jpeg]);
    assert_eq!(outcome.pairs[0].jpegs[0].role, JpegRole::SeparateShot);
}

#[test]
fn unsupported_file_kinds_are_reported() {
    let outcome = pair(&[file("notes.txt", &metadata("X", "Y", None, None, None))]);
    assert_eq!(outcome.unpaired.len(), 1);
    assert_eq!(
        outcome.unpaired[0].reason,
        UnpairedReason::UnsupportedFileKind
    );
}

#[test]
fn local_keys_reject_paths() {
    assert!(LocalImageKey::new("DSCF0100.RAF").is_some());
    assert!(LocalImageKey::new("../DSCF0100.RAF").is_none());
    assert!(LocalImageKey::new("a/b.RAF").is_none());
    assert!(LocalImageKey::new("a\\b.RAF").is_none());
    assert!(LocalImageKey::new("").is_none());
    let stem = LocalImageKey::new("x.RAF").expect("valid key");
    assert_eq!(stem.stem(), "x");
}
