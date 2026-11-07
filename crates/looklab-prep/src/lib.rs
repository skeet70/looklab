// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pairing, alignment, masking, density sampling and shuffling of RAW+JPEG pairs.
//! Reference preparation of [docs/03 §2.1](../../../docs/03-data-model.md); the `looklab-wasm` prep module is the deterministic reference build.

mod align;
pub mod chart;
mod error;
pub mod geom;
pub mod gradient;
mod image;
mod jpeg;
pub mod mask;
pub mod pair;
mod patch;
pub mod pattern;
pub mod pipeline;
pub mod preselect;
pub mod sample;
pub mod seed;

pub use align::{AlignParams, Alignment};
pub use chart::{
    ChartError, ChartExtraction, ChartLayout, ChartLayoutId, ChartParams, ChartRegion,
};
pub use error::PrepError;
pub use gradient::{GradientParams, GradientReport, GradientVerdict, detect_gradient};
pub use image::Image3;
pub use jpeg::{DecodedJpeg, decode_jpeg};
pub use mask::{Mask, MaskCounts, MaskOptions, build_mask};
pub use pair::{
    BodyKey, FileKind, JpegMember, JpegRole, LocalImageKey, PairingFile, PairingOutcome, RawFormat,
    RawPair, UnpairedFile, UnpairedReason, pair,
};
pub use pattern::{PatternDetection, PatternError, PatternParams, PatternSeed};
pub use pipeline::{
    AlignedRaw, PREP_VERSION, PrepParams, PreparedImage, PriorTransform, prepare_aligned_raw,
    prepare_decoded, prepare_decoded_with_chart, prepare_group, prepare_group_with_decoded_raw,
    prepare_pair, prepare_pair_decoded, prepare_pair_with_chart,
};
pub use preselect::{CoveragePool, PreselectOptions, preselect};
pub use sample::{MAX_DENSITY_SAMPLES, PixelSelection, SampleParams, select_pixels};
pub use seed::{SamplingSeed, SeedEntry, SeedFile};
