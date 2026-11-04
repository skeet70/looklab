// SPDX-License-Identifier: AGPL-3.0-or-later
//! rawler-backed decoder.

use crate::demosaic::{self, Crop, Levels, Pixels};
use crate::exif_orientation;
use crate::matrices::ColorMatrices;
use crate::orientation::Orientation;
use crate::{DecodedRaw, RawDecoder, RawError};
use rawler::RawImageData;
use rawler::decoders::RawDecodeParams;
use rawler::imgop::Rect;
use rawler::rawimage::{BlackLevel, RawImage, RawPhotometricInterpretation, WhiteLevel};

#[derive(Debug, Default, Clone, Copy)]
pub struct RawlerDecoder;

impl RawDecoder for RawlerDecoder {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedRaw, RawError> {
        if bytes.is_empty() {
            return Err(RawError::EmptyInput);
        }
        let source = rawler::rawsource::RawSource::new_from_slice(bytes);
        let image = rawler::decode(&source, &RawDecodeParams::default())
            .map_err(|err| RawError::Decode(err.to_string()))?;

        if image.cpp != 1 {
            return Err(RawError::Unsupported(
                "multi-component RAW data is not reduced",
            ));
        }
        let cfa = match &image.photometric {
            RawPhotometricInterpretation::Cfa(config) => &config.cfa,
            _ => {
                return Err(RawError::Unsupported(
                    "photometric interpretation is not a CFA",
                ));
            }
        };
        let crop = crop_of(&image);
        let pixels = match &image.data {
            RawImageData::Integer(values) => Pixels::U16(values),
            RawImageData::Float(values) => Pixels::F32(values),
        };
        let levels = levels_of(&image)?;
        let reduced = demosaic::reduce(pixels, image.width, image.height, cfa, crop, &levels)?;

        Ok(DecodedRaw {
            width: reduced.width,
            height: reduced.height,
            rgb: reduced.rgb,
            as_shot_wb: as_shot_wb(image.wb_coeffs),
            orientation: orientation_of(bytes, image.orientation),
            make: image.make,
            model: image.model,
            color_matrices: ColorMatrices::from_rawler(&image.color_matrix),
        })
    }
}

fn crop_of(image: &RawImage) -> Crop {
    if let Some(Rect { p, d }) = image.active_area {
        return Crop {
            x: p.x,
            y: p.y,
            width: d.w,
            height: d.h,
        };
    }
    // rawler omits active_area for some formats.
    Crop {
        x: 0,
        y: 0,
        width: image.width,
        height: image.height,
    }
}

fn levels_of(image: &RawImage) -> Result<Levels, RawError> {
    let black = image.blacklevel.as_vec();
    let (black_width, black_height) = black_tile(&image.blacklevel, black.len());
    let white = image.whitelevel.as_vec();
    let (white_width, white_height) = white_tile(&image.whitelevel, image);
    Levels::new(
        black,
        black_width,
        black_height,
        white,
        white_width,
        white_height,
    )
}

fn black_tile(levels: &BlackLevel, len: usize) -> (usize, usize) {
    if len == 1 {
        (1, 1)
    } else {
        (levels.width.max(1), levels.height.max(1))
    }
}

fn white_tile(levels: &WhiteLevel, image: &RawImage) -> (usize, usize) {
    let len = levels.0.len();
    let cfa = match &image.photometric {
        RawPhotometricInterpretation::Cfa(config) => Some(&config.cfa),
        _ => None,
    };
    match (len, cfa) {
        (1, _) => (1, 1),
        (_, Some(cfa)) if len == cfa.width * cfa.height => (cfa.width, cfa.height),
        (4, Some(cfa)) if cfa.width == 2 && cfa.height == 2 => (2, 2),
        // rawler supplies one white level for CFA data in every format reduced here.
        _ => (1, 1),
    }
}

fn as_shot_wb(coeffs: [f32; 4]) -> [f64; 3] {
    let green = coeffs[1];
    let usable =
        green.is_normal() && green != 0.0 && coeffs[0].is_finite() && coeffs[2].is_finite();
    if !usable {
        return [1.0, 1.0, 1.0];
    }
    [
        f64::from(coeffs[0]) / f64::from(green),
        1.0,
        f64::from(coeffs[2]) / f64::from(green),
    ]
}

fn orientation_of(bytes: &[u8], reported: rawler::Orientation) -> Orientation {
    exif_orientation::read_exif_orientation(bytes)
        .and_then(Orientation::from_exif)
        .unwrap_or_else(|| orientation_from_rawler(reported))
}

fn orientation_from_rawler(orientation: rawler::Orientation) -> Orientation {
    match orientation {
        rawler::Orientation::Normal => Orientation::Normal,
        rawler::Orientation::HorizontalFlip => Orientation::HorizontalFlip,
        rawler::Orientation::Rotate180 => Orientation::Rotate180,
        rawler::Orientation::VerticalFlip => Orientation::VerticalFlip,
        rawler::Orientation::Transpose => Orientation::Transpose,
        rawler::Orientation::Rotate90 => Orientation::Rotate90,
        rawler::Orientation::Transverse => Orientation::Transverse,
        rawler::Orientation::Rotate270 => Orientation::Rotate270,
        rawler::Orientation::Unknown => Orientation::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_shot_wb_is_normalized_to_green() {
        assert_eq!(as_shot_wb([2.0, 1.0, 1.5, f32::NAN]), [2.0, 1.0, 1.5]);
        assert_eq!(as_shot_wb([4.0, 2.0, 3.0, f32::NAN]), [2.0, 1.0, 1.5]);
        assert_eq!(as_shot_wb([f32::NAN; 4]), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn rawler_orientation_maps_to_exif() {
        assert_eq!(
            orientation_from_rawler(rawler::Orientation::Rotate90),
            Orientation::Rotate90
        );
        assert_eq!(
            orientation_from_rawler(rawler::Orientation::Unknown),
            Orientation::Normal
        );
    }
}
