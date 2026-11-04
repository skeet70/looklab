// SPDX-License-Identifier: AGPL-3.0-or-later
//! RAW decode wrapper around rawler; produces linear camera RGB.
//!
//! Rawler is the only LGPL dependency in the workspace and is isolated in this crate.
//!
//! # Output
//!
//! [`DecodedRaw::rgb`] is a reduced-size demosaic: each output pixel is the mean of one CFA
//! tile, a 2×2 quad for Bayer and a 6×6 tile for X-Trans. Samples are black-subtracted and
//! white-normalized with rawler's levels before averaging, and the image is cropped to
//! rawler's active area.
//!
//! # wasm32
//!
//! rawler needs `uuid/js` and getrandom's `wasm_js` backend to build for
//! `wasm32-unknown-unknown`; both are enabled from this crate's target-specific dependencies.
//! The reduced decode path spawns no rayon threads; the Node test fails if it does.

mod demosaic;
mod dng;
mod error;
mod exif_orientation;
mod matrices;
mod orientation;
mod rawler_decoder;

pub use error::RawError;
pub use matrices::{CameraMatrix, ColorMatrices, ForwardMatrix, Illuminant};
pub use orientation::Orientation;
pub use rawler_decoder::RawlerDecoder;

/// Bytes-to-image decoder.
pub trait RawDecoder {
    fn decode(&self, bytes: &[u8]) -> Result<DecodedRaw, RawError>;
}

/// A decoded RAW image, reduced to one linear camera-RGB sample per CFA tile.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedRaw {
    /// Active width / 2 for Bayer, / 6 for X-Trans.
    pub width: usize,
    /// Active height / 2 for Bayer, / 6 for X-Trans.
    pub height: usize,
    /// Linear camera RGB in sensor order, `width * height * 3` values, no white balance.
    pub rgb: Vec<f32>,
    /// As-shot multipliers `[R, G, B]` normalized to `G == 1`.
    pub as_shot_wb: [f64; 3],
    /// EXIF orientation; `rgb` stays in sensor order.
    pub orientation: Orientation,
    /// Make as reported by the file.
    pub make: String,
    /// Model as reported by the file.
    pub model: String,
    /// What rawler exposes for this file.
    pub color_matrices: Option<ColorMatrices>,
}

impl DecodedRaw {
    /// Pixel at `(x, y)` in sensor order, or `None` outside the image.
    pub fn pixel(&self, x: usize, y: usize) -> Option<[f32; 3]> {
        if x >= self.width || y >= self.height {
            return None;
        }
        let i = (y * self.width + x) * 3;
        let s = self.rgb.get(i..i + 3)?;
        Some([s[0], s[1], s[2]])
    }

    /// The image with as-shot white balance applied, i.e. `camera_rgb_wb_linear`.
    pub fn apply_wb(&self) -> Vec<f32> {
        apply_wb(&self.rgb, self.as_shot_wb)
    }
}

/// Multiply linear camera RGB by white-balance multipliers `[R, G, B]`.
pub fn apply_wb(rgb: &[f32], wb: [f64; 3]) -> Vec<f32> {
    rgb.iter()
        .enumerate()
        .map(|(i, v)| (f64::from(*v) * wb[i % 3]) as f32)
        .collect()
}

/// FNV-1a-64 over every value's `to_bits()` little-endian bytes, in order.
///
/// Used by the native and wasm32 decode tests to compare whole images with one number.
pub fn digest_f32(values: &[f32]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for value in values {
        for byte in value.to_bits().to_le_bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(PRIME);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_changes_with_a_bit() {
        assert_eq!(digest_f32(&[1.0, 2.0]), digest_f32(&[1.0, 2.0]));
        assert_ne!(digest_f32(&[1.0, 2.0]), digest_f32(&[1.0, 2.5]));
    }

    #[test]
    fn apply_wb_scales_each_channel() {
        let out = apply_wb(&[0.5, 0.5, 0.5, 0.25, 0.25, 0.25], [2.0, 1.0, 0.5]);
        assert_eq!(out, vec![1.0, 0.5, 0.25, 0.5, 0.25, 0.125]);
    }
}
