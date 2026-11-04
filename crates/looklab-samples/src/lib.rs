// SPDX-License-Identifier: AGPL-3.0-or-later
//! Versioned sample blob format with bounded readers.
//!
//! Implements the version 1 layout of [docs/03 §2.2](../../../docs/03-data-model.md)
//! and the patch-table blob that follows it. Both blobs are
//! `magic ‖ version ‖ header_len ‖ postcard header ‖ zstd body`, and both
//! readers bound every allocation before touching the body.
//!
//! ## Value limits (version 1)
//!
//! - `in_rgb`, `out_rgb` and patch-table means: finite, in
//!   [`MIN_RGB`]`..=`[`MAX_RGB`] (`-0.5 ..= 64.0`).
//! - `density_w`: finite, greater than zero, at most [`MAX_DENSITY_WEIGHT`].
//! - patch-table standard deviations: finite, `0 ..= `[`MAX_RGB`].
//! - `wb_multipliers`: finite, in [`MIN_WB_MULTIPLIER`]`..=`[`MAX_WB_MULTIPLIER`].
//! - `cct_hint_k`: finite, in [`MIN_CCT_HINT_K`]`..=`[`MAX_CCT_HINT_K`]; zero
//!   means unknown.
//! - `exposure_hint_ev`: finite, magnitude at most [`MAX_EXPOSURE_HINT_EV`].
//! - sample counts and patch-row counts: `1 ..= `[`MAX_SAMPLES`] /
//!   [`MAX_PATCHES`].
//! - an encoded header is at most [`MAX_HEADER_LEN`] bytes; a zstd frame
//!   either declares its content size, which must equal `n × 29`, or needs a
//!   window of at most [`MAX_WINDOW_BYTES`]. Decompression collects at most
//!   `n × 29 + 1024` bytes, and the codec holds at most one decoded block,
//!   bounded by the frame window.
//!
//! ## Errors
//!
//! [`Error`] has one variant per rejection reason.

mod blob;
pub mod error;
mod patch;
mod sample;
mod types;
mod zstd_frame;

pub use error::{BlobKind, Error};
pub use patch::{
    MAGIC as PATCH_MAGIC, PatchId, PatchRow, PatchTable, PatchTableHeader,
    ROW_BYTES as PATCH_ROW_BYTES, VERSION as PATCH_VERSION, decode_patch_table, encode_patch_table,
};
pub use sample::{MAGIC as SAMPLE_MAGIC, VERSION as SAMPLE_VERSION, decode, encode};
pub use types::{
    Field, InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion,
    RerenderGroupId, SampleFlags, SampleHeader, SampleSet,
};

pub const MAX_SAMPLES: u32 = 65_536;

pub const MAX_PATCHES: u32 = 65_536;

pub const MAX_HEADER_LEN: u32 = 4096;

/// `3 × f32` in, `3 × f32` out, `f32` weight, `u8` flags.
pub const ROW_BYTES: usize = 3 * 4 + 3 * 4 + 4 + 1;

/// Allowance on top of `n × ROW_BYTES` before the reader calls a body oversized.
pub const DECOMPRESS_HEADROOM: usize = 1024;

pub const MIN_RGB: f32 = -0.5;

pub const MAX_RGB: f32 = 64.0;

pub const MAX_DENSITY_WEIGHT: f32 = 1.0e6;

pub const MIN_WB_MULTIPLIER: f32 = 0.01;

pub const MAX_WB_MULTIPLIER: f32 = 64.0;

/// Zero means the hint is unknown.
pub const MIN_CCT_HINT_K: f32 = 0.0;

pub const MAX_CCT_HINT_K: f32 = 100_000.0;

pub const MAX_EXPOSURE_HINT_EV: f32 = 16.0;

pub const MAX_WINDOW_BYTES: u64 = 1 << 20;
