// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_meta::{
    CaptureTime, ExifData, FujiMakernote, ImageNumber, LookSettings, ParsedMetadata, ParsedTag,
    SubSec,
};
use looklab_prep::{CameraIdentity, JpegRole, LocalImageKey, PairingFile, UnpairedReason, pair};

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
        firmware: Some("1.00".to_owned()),
        render_tag: looklab_meta::RenderTagOutcome::Absent,
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

fn with_saturation(
    mut metadata: ParsedMetadata,
    saturation: looklab_meta::ColorSaturation,
) -> ParsedMetadata {
    metadata
        .fuji
        .as_mut()
        .expect("the fixture has makernotes")
        .look
        .saturation = Some(ParsedTag {
        tag: looklab_meta::FujiTag::Saturation,
        raw: 0,
        value: Some(saturation),
    });
    metadata
}

#[test]
fn same_raw_keeps_provia_velvia_and_acros_as_separate_renders() {
    let render = |name: &str, film: looklab_meta::FilmMode| {
        file(
            name,
            &metadata(
                "FUJIFILM",
                "X-S10",
                Some(capture(9, None)),
                Some(16),
                Some(film),
            ),
        )
    };
    let raw = file(
        "DSCF0106.RAF",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(9, None)),
            Some(16),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let provia = render("RENDER-1.JPG", looklab_meta::FilmMode::Provia);
    let velvia = render("RENDER-2.JPG", looklab_meta::FilmMode::Velvia);
    let acros = file(
        "RENDER-3.JPG",
        &with_saturation(
            metadata(
                "FUJIFILM",
                "X-S10",
                Some(capture(9, None)),
                Some(16),
                Some(looklab_meta::FilmMode::Provia),
            ),
            looklab_meta::ColorSaturation::Acros(looklab_meta::MonochromeFilter::NoFilter),
        ),
    );
    let outcome = pair(&[raw, provia, velvia, acros]);

    assert_eq!(outcome.pairs.len(), 1);
    assert_eq!(
        outcome.pairs[0]
            .rerender_members()
            .cloned()
            .collect::<Vec<_>>(),
        vec![
            key("RENDER-1.JPG"),
            key("RENDER-2.JPG"),
            key("RENDER-3.JPG")
        ]
    );
    let groups = outcome.rerender_groups();
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].jpegs.len(), 3);
}

#[test]
fn same_raw_keeps_two_settings_of_one_sim_as_separate_renders() {
    let raw = file(
        "DSCF0107.RAF",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(10, None)),
            Some(17),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let neutral = file(
        "RENDER-1.JPG",
        &metadata(
            "FUJIFILM",
            "X-S10",
            Some(capture(10, None)),
            Some(17),
            Some(looklab_meta::FilmMode::Provia),
        ),
    );
    let punchy = file(
        "RENDER-2.JPG",
        &with_saturation(
            metadata(
                "FUJIFILM",
                "X-S10",
                Some(capture(10, None)),
                Some(17),
                Some(looklab_meta::FilmMode::Provia),
            ),
            looklab_meta::ColorSaturation::Step(2),
        ),
    );
    let outcome = pair(&[raw, neutral, punchy]);

    assert_eq!(
        outcome.pairs[0]
            .rerender_members()
            .cloned()
            .collect::<Vec<_>>(),
        vec![key("RENDER-1.JPG"), key("RENDER-2.JPG")]
    );
    assert!(
        outcome.pairs[0]
            .jpegs
            .iter()
            .all(|member| member.role == JpegRole::Rerender)
    );
}

#[test]
fn different_or_unknown_rendering_firmware_does_not_discard_a_jpeg() {
    for (left_firmware, right_firmware) in [
        (Some("1.00"), Some("1.10")),
        (None, Some("1.00")),
        (None, None),
    ] {
        let make_metadata = || {
            metadata(
                "FUJIFILM",
                "X-S10",
                Some(capture(8, None)),
                Some(15),
                Some(looklab_meta::FilmMode::Provia),
            )
        };
        let raw = file("DSCF0105.RAF", &make_metadata());
        let mut left = make_metadata();
        left.firmware = left_firmware.map(str::to_owned);
        let mut right = make_metadata();
        right.firmware = right_firmware.map(str::to_owned);
        let outcome = pair(&[
            raw,
            file("DSCF0105-1.JPG", &left),
            file("DSCF0105-2.JPG", &right),
        ]);
        assert_eq!(
            outcome.pairs[0]
                .rerender_members()
                .cloned()
                .collect::<Vec<_>>(),
            vec![key("DSCF0105-1.JPG"), key("DSCF0105-2.JPG")],
            "rendering firmware {left_firmware:?} / {right_firmware:?}",
        );
    }
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
fn a_raw_model_that_repeats_the_make_pairs_with_the_bare_model() {
    let raw = file(
        "B_31325.3FR",
        &metadata(
            "Hasselblad",
            "Hasselblad X2D II 100C",
            Some(capture(9, None)),
            None,
            None,
        ),
    );
    let jpeg = file(
        "B_31325.JPG",
        &metadata(
            "Hasselblad",
            "X2D II 100C",
            Some(capture(9, None)),
            None,
            None,
        ),
    );
    let outcome = pair(&[raw, jpeg]);
    assert_eq!(outcome.pairs.len(), 1);
    assert_eq!(outcome.pairs[0].raw, key("B_31325.3FR"));
    assert_eq!(outcome.pairs[0].jpegs[0].key, key("B_31325.JPG"));
    assert!(outcome.unpaired.is_empty());
}

#[test]
fn a_model_that_repeats_the_make_normalises_to_the_bare_model() {
    let hasselblad = CameraIdentity::new("Hasselblad", "Hasselblad X2D II 100C").expect("identity");
    assert_eq!(hasselblad.make(), "hasselblad");
    assert_eq!(hasselblad.model(), "x2d ii 100c");
    assert!(hasselblad.matches("Hasselblad", "X2D II 100C"));
    assert!(hasselblad.matches("  hasselblad ", " HASSELBLAD X2D II 100C "));
    assert!(!hasselblad.matches("Hasselblad", "X1D II 50C"));

    let fuji = CameraIdentity::new("FUJIFILM", "X-S10").expect("identity");
    assert_eq!((fuji.make(), fuji.model()), ("fujifilm", "x-s10"));
    assert!(fuji.matches("Fujifilm", "X-S10"));
    assert!(!fuji.matches("FUJIFILM", "X-S10 II"));

    let same = CameraIdentity::new("Hasselblad", "Hasselblad").expect("identity");
    assert_eq!(same.model(), "hasselblad");
    assert!(same.matches("Hasselblad", "Hasselblad"));
}

#[test]
fn camera_matching_keeps_make_and_model_separate() {
    let identity = CameraIdentity::new("Maker Camera", "Model").expect("identity");
    assert!(identity.matches("MAKER CAMERA", "Model"));
    assert!(!identity.matches("Maker", "Camera Model"));
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
