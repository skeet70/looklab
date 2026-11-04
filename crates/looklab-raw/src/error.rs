// SPDX-License-Identifier: AGPL-3.0-or-later
//! Decode errors.

#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("empty input")]
    EmptyInput,
    /// rawler has no decoder for this camera or mode.
    #[error("rawler does not support {make} {model}: {what} (mode {mode})")]
    UnsupportedCamera {
        what: String,
        make: String,
        model: String,
        mode: String,
    },
    /// rawler's category for corrupt or undecodable image data.
    #[error("rawler could not decode the image: {detail}")]
    DecodeFailed { detail: String },
    #[error("{cpp} components per pixel are not reduced")]
    MultiComponent { cpp: usize },
    #[error("photometric interpretation is not a CFA")]
    NotCfa,
    #[error("CFA {width}x{height} is neither Bayer 2x2 nor X-Trans 6x6")]
    UnsupportedCfa { width: usize, height: usize },
    #[error("CFA has colours other than R, G and B")]
    NonRgbCfa,
    #[error("image {width}x{height} has no pixels")]
    EmptyImage { width: usize, height: usize },
    #[error("{samples} samples for a {width}x{height} image")]
    SampleCount {
        samples: usize,
        width: usize,
        height: usize,
    },
    #[error("crop {crop_width}x{crop_height} at ({x}, {y}) exceeds {image_width}x{image_height}")]
    CropOutside {
        x: usize,
        y: usize,
        crop_width: usize,
        crop_height: usize,
        image_width: usize,
        image_height: usize,
    },
    #[error("crop {width}x{height} is smaller than one {tile}x{tile} tile")]
    CropTooSmall {
        width: usize,
        height: usize,
        tile: usize,
    },
    #[error("empty black or white level table")]
    EmptyLevels,
    #[error("{levels} black levels for a {width}x{height} tile")]
    BlackLevelCount {
        levels: usize,
        width: usize,
        height: usize,
    },
    #[error("{levels} white levels for a {width}x{height} tile")]
    WhiteLevelCount {
        levels: usize,
        width: usize,
        height: usize,
    },
    #[error("non-finite black or white level")]
    NonFiniteLevel,
    #[error("white level {white} is not above black level {black}")]
    WhiteNotAboveBlack { white: f32, black: f32 },
}
