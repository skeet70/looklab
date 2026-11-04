// SPDX-License-Identifier: AGPL-3.0-or-later
//! The capture tuple trust merges honest duplicates on, rebuilt from stored metadata.

use looklab_meta::{
    Aperture, ExposureCompensation, ExposureTime, FocalLength, Iso, SanitizedMetadata,
    WhiteBalanceMultipliers as MetaWhiteBalance, YearMonth,
};
use looklab_trust::CaptureTuple;
use looklab_upload::ManifestMetadata;

use crate::error::Result;

pub fn capture_tuple(meta_json: &str) -> Result<CaptureTuple> {
    let meta: ManifestMetadata = serde_json::from_str(meta_json)?;
    let sanitized = SanitizedMetadata {
        make: meta.make,
        model: meta.model,
        firmware: meta.firmware,
        lens_model: meta.lens_model,
        focal_length: meta.focal_length_mm.and_then(FocalLength::new),
        iso: meta.iso.and_then(Iso::new),
        shutter: meta.shutter_seconds.and_then(ExposureTime::new),
        aperture: meta.aperture_f_number.and_then(Aperture::new),
        exposure_compensation: meta
            .exposure_compensation_ev
            .and_then(ExposureCompensation::new),
        as_shot_wb: meta.as_shot_wb.map(|wb| MetaWhiteBalance {
            r: wb.r,
            g: wb.g,
            b: wb.b,
        }),
        capture_month: meta
            .capture_month
            .and_then(|month| YearMonth::new(month.year(), month.month())),
        software: meta.software,
        // The wire mirror keeps no FujiTag, so the in-camera look slot cannot be rebuilt.
        look: None,
    };
    Ok(CaptureTuple::from_metadata(&sanitized)?)
}
