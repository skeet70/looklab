// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tile-averaging reduction of a CFA mosaic to linear camera RGB.

use crate::RawError;
use rawler::CFA;

pub(crate) enum Pixels<'a> {
    U16(&'a [u16]),
    F32(&'a [f32]),
}

impl Pixels<'_> {
    fn len(&self) -> usize {
        match self {
            Self::U16(values) => values.len(),
            Self::F32(values) => values.len(),
        }
    }

    fn at(&self, index: usize) -> f32 {
        match self {
            Self::U16(values) => f32::from(values[index]),
            Self::F32(values) => values[index],
        }
    }
}

/// Black and white levels as samples per repeating tile.
pub(crate) struct Levels {
    black: Vec<f32>,
    black_width: usize,
    black_height: usize,
    white: Vec<f32>,
    white_width: usize,
    white_height: usize,
}

impl Levels {
    pub(crate) fn new(
        black: Vec<f32>,
        black_width: usize,
        black_height: usize,
        white: Vec<f32>,
        white_width: usize,
        white_height: usize,
    ) -> Result<Self, RawError> {
        if black.is_empty() || white.is_empty() {
            return Err(RawError::Levels("empty level table".into()));
        }
        if black.len() != 1 && black.len() != black_width * black_height {
            return Err(RawError::Levels(format!(
                "{} black levels for a {black_width}x{black_height} tile",
                black.len()
            )));
        }
        if white.len() != 1 && white.len() != white_width * white_height {
            return Err(RawError::Levels(format!(
                "{} white levels for a {white_width}x{white_height} tile",
                white.len()
            )));
        }
        if black.iter().chain(&white).any(|v| !v.is_finite()) {
            return Err(RawError::Levels("non-finite level".into()));
        }
        let max_black = black.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let min_white = white.iter().copied().fold(f32::INFINITY, f32::min);
        if min_white <= max_black {
            return Err(RawError::Levels(format!(
                "white level {min_white} is not above black level {max_black}"
            )));
        }
        Ok(Self {
            black,
            black_width,
            black_height,
            white,
            white_width,
            white_height,
        })
    }

    fn black_at(&self, x: usize, y: usize) -> f32 {
        let index = if self.black.len() == 1 {
            0
        } else {
            (y % self.black_height) * self.black_width + x % self.black_width
        };
        self.black[index]
    }

    fn white_at(&self, x: usize, y: usize) -> f32 {
        let index = if self.white.len() == 1 {
            0
        } else {
            (y % self.white_height) * self.white_width + x % self.white_width
        };
        self.white[index]
    }
}

#[derive(Debug)]
pub(crate) struct Reduced {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) rgb: Vec<f32>,
}

/// Crop rectangle in sensor coordinates: `(x, y, width, height)`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Crop {
    pub(crate) x: usize,
    pub(crate) y: usize,
    pub(crate) width: usize,
    pub(crate) height: usize,
}

/// Average every `tile × tile` CFA block into one RGB pixel.
///
/// A 6×6 X-Trans tile contains 8 red, 20 green and 8 blue samples; a 2×2 Bayer quad contains one
/// red, two green and one blue. Per-colour means therefore weight the two green sites equally.
pub(crate) fn reduce(
    pixels: Pixels<'_>,
    width: usize,
    height: usize,
    cfa: &CFA,
    crop: Crop,
    levels: &Levels,
) -> Result<Reduced, RawError> {
    let tile = match (cfa.width, cfa.height) {
        (2, 2) => 2,
        (6, 6) => 6,
        _ => {
            return Err(RawError::Unsupported(
                "CFA tile is neither Bayer 2x2 nor X-Trans 6x6",
            ));
        }
    };
    if width == 0 || height == 0 || crop.width == 0 || crop.height == 0 {
        return Err(RawError::Geometry(format!(
            "{width}x{height} image, {}x{} crop",
            crop.width, crop.height
        )));
    }
    if crop.x + crop.width > width || crop.y + crop.height > height {
        return Err(RawError::Geometry(format!(
            "{}x{} crop at {},{} exceeds {width}x{height}",
            crop.width, crop.height, crop.x, crop.y
        )));
    }
    if pixels.len() < width * height {
        return Err(RawError::Geometry(format!(
            "{} samples for {width}x{height}",
            pixels.len()
        )));
    }
    for y in 0..cfa.height {
        for x in 0..cfa.width {
            if cfa.color_at(y, x) > 2 {
                return Err(RawError::Unsupported(
                    "CFA has colours other than R, G and B",
                ));
            }
        }
    }

    let out_width = crop.width / tile;
    let out_height = crop.height / tile;
    if out_width == 0 || out_height == 0 {
        return Err(RawError::Geometry(format!(
            "crop {}x{} is smaller than one {tile}x{tile} tile",
            crop.width, crop.height
        )));
    }

    let mut rgb = Vec::with_capacity(out_width * out_height * 3);
    for block_y in 0..out_height {
        let y0 = crop.y + block_y * tile;
        for block_x in 0..out_width {
            let x0 = crop.x + block_x * tile;
            let mut sums = [0.0f32; 3];
            let mut counts = [0u32; 3];
            for y in y0..y0 + tile {
                let row = y * width;
                for x in x0..x0 + tile {
                    let color = cfa.color_at(y, x);
                    let black = levels.black_at(x, y);
                    let white = levels.white_at(x, y);
                    let value = (pixels.at(row + x) - black) / (white - black);
                    sums[color] += value.max(0.0);
                    counts[color] += 1;
                }
            }
            for color in 0..3 {
                rgb.push(if counts[color] == 0 {
                    0.0
                } else {
                    sums[color] / counts[color] as f32
                });
            }
        }
    }

    Ok(Reduced {
        width: out_width,
        height: out_height,
        rgb,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bayer_levels() -> Levels {
        Levels::new(vec![0.0, 0.0, 0.0, 0.0], 2, 2, vec![16.0], 1, 1).expect("levels")
    }

    #[test]
    fn bayer_quad_averages_each_colour() {
        // RGGB: R=4, G1=8, G2=12, B=16.
        let pixels = vec![4u16, 8, 12, 16];
        let cfa = CFA::new("RGGB");
        let crop = Crop {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let reduced =
            reduce(Pixels::U16(&pixels), 2, 2, &cfa, crop, &bayer_levels()).expect("reduce");
        assert_eq!((reduced.width, reduced.height), (1, 1));
        assert_eq!(reduced.rgb, vec![0.25, 0.625, 1.0]);
    }

    #[test]
    fn black_level_is_subtracted_before_normalizing() {
        // RGGB with a 4 count black on every position and a white of 20.
        let levels = Levels::new(vec![4.0], 1, 1, vec![20.0], 1, 1).expect("levels");
        let pixels = vec![4u16, 8, 12, 24];
        let cfa = CFA::new("RGGB");
        let crop = Crop {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let reduced = reduce(Pixels::U16(&pixels), 2, 2, &cfa, crop, &levels).expect("reduce");
        assert_eq!(reduced.rgb, vec![0.0, 0.375, 1.25]);
    }

    #[test]
    fn odd_crop_drops_the_partial_tile() {
        let pixels = vec![0u16; 3 * 3];
        let cfa = CFA::new("RGGB");
        let crop = Crop {
            x: 0,
            y: 0,
            width: 3,
            height: 3,
        };
        let reduced =
            reduce(Pixels::U16(&pixels), 3, 3, &cfa, crop, &bayer_levels()).expect("reduce");
        assert_eq!((reduced.width, reduced.height), (1, 1));
    }

    #[test]
    fn rejects_a_crop_outside_the_image() {
        let pixels = vec![0u16; 4];
        let cfa = CFA::new("RGGB");
        let crop = Crop {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        };
        let err =
            reduce(Pixels::U16(&pixels), 2, 2, &cfa, crop, &bayer_levels()).expect_err("crop");
        assert!(matches!(err, RawError::Geometry(_)));
    }
}
