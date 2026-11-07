// SPDX-License-Identifier: AGPL-3.0-or-later
//! rawler-backed decoder.

use crate::demosaic::{self, BlackOrigin, Crop, Levels, Pixels};
use crate::dng;
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
        let image =
            rawler::decode(&source, &RawDecodeParams::default()).map_err(|err| match err {
                rawler::RawlerError::Unsupported {
                    what,
                    make,
                    model,
                    mode,
                } => RawError::UnsupportedCamera {
                    what,
                    make,
                    model,
                    mode,
                },
                rawler::RawlerError::DecoderFailed(detail) => RawError::DecodeFailed { detail },
            })?;
        drop(source);

        if image.cpp != 1 {
            return Err(RawError::MultiComponent { cpp: image.cpp });
        }
        let cfa = match &image.photometric {
            RawPhotometricInterpretation::Cfa(config) => &config.cfa,
            _ => return Err(RawError::NotCfa),
        };
        let crop = crop_of(&image);
        let pixels = match &image.data {
            RawImageData::Integer(values) => Pixels::U16(values.as_slice()),
            RawImageData::Float(values) => Pixels::F32(values.as_slice()),
        };
        let dng = dng::inspect(bytes);
        let black_origin = if dng.is_dng {
            BlackOrigin::ActiveArea
        } else {
            BlackOrigin::Sensor
        };
        let levels = levels_of(&image, black_origin)?;
        let reduced = demosaic::reduce(pixels, image.width, image.height, cfa, crop, &levels)?;

        Ok(DecodedRaw {
            width: reduced.width,
            height: reduced.height,
            rgb: reduced.rgb,
            as_shot_wb: as_shot_wb(image.wb_coeffs),
            orientation: orientation_of(bytes, image.orientation),
            make: image.make,
            model: image.model,
            color_matrices: ColorMatrices::new(&image.color_matrix, dng.forward_matrices),
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

fn levels_of(image: &RawImage, black_origin: BlackOrigin) -> Result<Levels, RawError> {
    let black = image.blacklevel.as_vec();
    let (black_width, black_height) = black_tile(&image.blacklevel, black.len());
    let white = image.whitelevel.as_vec();
    let (white_width, white_height) = white_tile(&image.whitelevel, image);
    Levels::new(
        black,
        black_width,
        black_height,
        black_origin,
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
    let usable = green.is_normal() && coeffs[0].is_finite() && coeffs[2].is_finite();
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
    exif_orientation::main_orientation(bytes)
        .or_else(|| exif_orientation::embedded_exif_orientation(bytes))
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

    fn tiff_with_orientation(value: u16) -> Vec<u8> {
        let mut tiff = b"II*\0\x08\0\0\0".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&value.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes());
        tiff.extend_from_slice(&0u32.to_le_bytes());
        tiff
    }

    #[test]
    fn main_tiff_orientation_beats_a_preview_block() {
        let mut bytes = tiff_with_orientation(6);
        bytes.extend_from_slice(b"Exif\0\0");
        bytes.extend_from_slice(&tiff_with_orientation(3));
        assert_eq!(
            orientation_of(&bytes, rawler::Orientation::Normal),
            Orientation::Rotate90
        );
    }

    #[test]
    fn raf_uses_the_embedded_preview_orientation() {
        let mut bytes = b"FUJIFILMCCD-RAW".to_vec();
        bytes.extend_from_slice(b"junk Exif\0\0");
        bytes.extend_from_slice(&tiff_with_orientation(3));
        assert_eq!(
            orientation_of(&bytes, rawler::Orientation::Normal),
            Orientation::Rotate180
        );
    }
}
