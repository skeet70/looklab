// SPDX-License-Identifier: AGPL-3.0-or-later

mod common;

use looklab_meta::{
    Aperture, ExposureCompensation, ExposureTime, FilmMode, FocalLength, FujiTag, Iso,
    LookSettings, ParsedPair, ParsedTag, SanitizedMetadata, WhiteBalanceFineTune,
    WhiteBalanceMultipliers, YearMonth,
};
use looklab_upload::{
    BatchManifest, BlobKind, FilmMode as WireFilmMode, ManifestImage, ManifestMetadata,
    RerenderGroupId, canonical_manifest_bytes,
};
use sha2::{Digest, Sha256};

fn sanitized() -> SanitizedMetadata {
    SanitizedMetadata {
        make: Some("Fujifilm".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some("2.10".to_owned()),
        lens_model: Some("XF 23mm F1.4".to_owned()),
        focal_length: FocalLength::new(23.0),
        iso: Iso::new(400),
        shutter: ExposureTime::new(0.008),
        aperture: Aperture::new(1.4),
        exposure_compensation: ExposureCompensation::new(-0.33),
        as_shot_wb: Some(WhiteBalanceMultipliers {
            r: 2.0,
            g: 1.0,
            b: 1.5,
        }),
        capture_month: YearMonth::new(2024, 5),
        software: Some("X-T5 Ver.2.10".to_owned()),
        look: Some(LookSettings {
            film_mode: Some(ParsedTag {
                tag: FujiTag::FilmMode,
                raw: 0x0700,
                value: Some(FilmMode::ClassicChrome),
            }),
            white_balance_fine_tune: Some(ParsedPair {
                tag: FujiTag::WhiteBalanceFineTune,
                raw: [20, -10],
                value: Some(WhiteBalanceFineTune { red: 20, blue: -10 }),
            }),
            ..LookSettings::default()
        }),
    }
}

#[test]
fn mirror_carries_every_sanitized_field() {
    let mirror = ManifestMetadata::from(&sanitized());
    assert_eq!(mirror.make.as_deref(), Some("Fujifilm"));
    assert_eq!(mirror.model.as_deref(), Some("X-T5"));
    assert_eq!(mirror.firmware.as_deref(), Some("2.10"));
    assert_eq!(mirror.lens_model.as_deref(), Some("XF 23mm F1.4"));
    assert_eq!(mirror.focal_length_mm, Some(23.0));
    assert_eq!(mirror.iso, Some(400));
    assert_eq!(mirror.shutter_seconds, Some(0.008));
    assert_eq!(mirror.aperture_f_number, Some(1.4));
    assert_eq!(mirror.exposure_compensation_ev, Some(-0.33));
    assert_eq!(
        mirror.as_shot_wb,
        Some(looklab_upload::WhiteBalanceMultipliers {
            r: 2.0,
            g: 1.0,
            b: 1.5
        })
    );
    let month = mirror.capture_month.expect("month survives the mirror");
    assert_eq!((month.year(), month.month()), (2024, 5));

    let look = mirror.look.expect("look settings survive the mirror");
    let film = look.film_mode.expect("film mode survives");
    assert_eq!(film.raw, 0x0700);
    assert_eq!(film.value, Some(WireFilmMode::ClassicChrome));
    let fine_tune = look.white_balance_fine_tune.expect("fine tune survives");
    assert_eq!(fine_tune.raw, [20, -10]);
    let shift = fine_tune.shift.expect("decoded fine tune survives");
    assert_eq!((shift.red, shift.blue), (20, -10));
}

#[test]
fn manifest_with_metadata_roundtrips_through_postcard() {
    let blob = common::sample_blob();
    let image = ManifestImage {
        meta: ManifestMetadata::from(&sanitized()),
        rerender_group: Some(RerenderGroupId::new([4; 16])),
        blob_sha256: Sha256::digest(&blob).into(),
        blob_kind: BlobKind::Samples,
    };
    let manifest = common::manifest(vec![image]);
    let bytes = canonical_manifest_bytes(&manifest);
    let (decoded, rest) =
        postcard::take_from_bytes::<BatchManifest>(&bytes).expect("canonical manifest decodes");
    assert!(rest.is_empty());
    assert_eq!(decoded, manifest);
}
