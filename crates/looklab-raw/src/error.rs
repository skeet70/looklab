// SPDX-License-Identifier: AGPL-3.0-or-later
//! Decode errors.

#[derive(Debug, thiserror::Error)]
pub enum RawError {
    #[error("empty input")]
    EmptyInput,
    /// rawler's error stays a `String` so its type is not part of this crate's API.
    #[error("rawler could not decode the file: {0}")]
    Decode(String),
    #[error("unsupported RAW layout: {0}")]
    Unsupported(&'static str),
    #[error("invalid image geometry: {0}")]
    Geometry(String),
    #[error("invalid black/white levels: {0}")]
    Levels(String),
}
