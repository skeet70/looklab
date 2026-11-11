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
        render_tag: meta.render_tag,
        render_tag_malformed: meta.render_tag_malformed,
        // The typed look needs FujiTag values the wire mirror does not keep; the tuple
        // takes the mirror itself.
        look: None,
    };
    Ok(CaptureTuple::from_metadata(&sanitized)?.with_wire_look(meta.look.as_ref())?)
}

#[cfg(test)]
mod tests {
    use super::capture_tuple;

    fn record(film_mode: &str, raw: i64) -> String {
        format!(
            r#"{{"model":"X-T4","iso":160,"shutter_seconds":0.0166,"look":{{"film_mode":{{"raw":{raw},"value":"{film_mode}"}}}}}}"#
        )
    }

    #[test]
    fn rerenders_of_one_capture_with_different_settings_are_not_duplicates() {
        let astia = capture_tuple(&record("Astia", 288)).expect("astia");
        let velvia = capture_tuple(&record("Velvia", 512)).expect("velvia");
        assert_ne!(astia, velvia);
        assert_eq!(
            astia,
            capture_tuple(&record("Astia", 288)).expect("repeat upload")
        );
    }
}
