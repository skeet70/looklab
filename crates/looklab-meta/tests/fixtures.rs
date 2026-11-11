// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(clippy::disallowed_methods)]
mod support;

use std::path::{Path, PathBuf};

use looklab_meta::{DynamicRange, FilmMode, RemovedField, WhiteBalanceMode};

#[test]
fn fixtures_parse_and_sanitize() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures");
    if !directory.is_dir() {
        return;
    }
    let mut files = Vec::new();
    collect(&directory, &mut files);
    files.sort();

    let mut checked = 0;
    for path in files {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        if !matches!(
            extension.as_deref(),
            Some("raf" | "jpg" | "jpeg" | "tif" | "tiff" | "dng")
        ) {
            continue;
        }
        let bytes = std::fs::read(&path).expect("fixture is readable");
        let parsed = looklab_meta::parse(&bytes)
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        let (sanitized, _) = looklab_meta::sanitize(&parsed);
        let json = serde_json::to_string(&sanitized).expect("sanitized metadata is JSON");
        support::assert_whitelisted_keys(&json);
        checked += 1;
    }
    if checked == 0 {
        eprintln!("no metadata fixtures present");
    }
}

#[test]
fn the_owner_fixture_extracts_makernote_settings() {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cache/fujifilm-x-s10.raf");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let parsed = looklab_meta::parse(&bytes).expect("RAF parses");
    let (sanitized, removed) = looklab_meta::sanitize(&parsed);

    assert_eq!(sanitized.make.as_deref(), Some("FUJIFILM"));
    assert_eq!(sanitized.model.as_deref(), Some("X-S10"));
    assert_eq!(sanitized.firmware.as_deref(), Some("1.00"));
    assert_eq!(
        sanitized.focal_length.expect("focal length").millimetres(),
        69.8
    );
    assert_eq!(sanitized.iso.expect("ISO").value(), 160);
    assert_eq!(
        sanitized.capture_month.expect("capture month").to_string(),
        "2020-11"
    );
    let look = sanitized.look.expect("makernote look settings");
    assert_eq!(
        look.film_mode.expect("film mode").value,
        Some(FilmMode::Provia)
    );
    assert_eq!(
        look.dynamic_range.expect("dynamic range").value,
        Some(DynamicRange::Standard)
    );
    assert_eq!(
        look.white_balance.expect("white balance").value,
        Some(WhiteBalanceMode::Auto)
    );
    assert!(removed.contains(RemovedField::BodySerialNumber));
    assert!(removed.contains(RemovedField::InternalSerialNumber));
    assert!(removed.contains(RemovedField::MakerNote));
}

#[test]
fn a_non_owner_fixture_extracts_exif() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/cache/scene-1955.dng");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let parsed = looklab_meta::parse(&bytes).expect("DNG parses");
    let (sanitized, removed) = looklab_meta::sanitize(&parsed);

    assert_eq!(
        sanitized.make.as_deref(),
        Some("RICOH IMAGING COMPANY, LTD.")
    );
    assert_eq!(sanitized.model.as_deref(), Some("GR II"));
    assert_eq!(sanitized.iso.expect("ISO").value(), 400);
    assert_eq!(sanitized.aperture.expect("aperture").f_number(), 8.0);
    assert_eq!(
        sanitized.capture_month.expect("capture month").to_string(),
        "2017-02"
    );
    assert!(sanitized.look.is_none());
    assert!(removed.contains(RemovedField::GpsLocation));
}

fn collect(directory: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else {
            files.push(path);
        }
    }
}
