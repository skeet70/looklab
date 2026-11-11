// SPDX-License-Identifier: AGPL-3.0-or-later

use serde::{Deserialize, Serialize};

use crate::Function;

/// Nested library errors cross this boundary as messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ModuleError {
    #[error("unknown function id {fn_id}")]
    UnknownFunction { fn_id: u32 },
    #[error("{function:?} request is not valid postcard: {message}")]
    Request { function: Function, message: String },
    #[error("preparation failed: {message}")]
    Prep { message: String },
    #[error("the RAW reports no camera colour matrices")]
    NoColorMatrices,
    #[error("metadata for manifest image {index} is invalid: {message}")]
    Metadata { index: usize, message: String },
    #[error("manifest is not a canonical BatchManifest: {message}")]
    Manifest { message: String },
    #[error("send manifest is not canonical: {message}")]
    SendManifest { message: String },
    #[error("proof-of-work failed: {message}")]
    Pow { message: String },
    #[error("envelope encoding failed: {message}")]
    EncodeEnvelope { message: String },
    #[error("envelope parsing failed: {message}")]
    ParseEnvelope { message: String },
    #[error("blob count {count} does not match the {images} manifest images")]
    BlobCount { count: usize, images: usize },
    #[error("proof-of-work has {found} solutions, expected {expected}")]
    PowCount { found: usize, expected: usize },
}

impl ModuleError {
    pub(crate) fn prep(error: impl core::fmt::Display) -> Self {
        Self::Prep {
            message: error.to_string(),
        }
    }

    pub(crate) fn pow(error: impl core::fmt::Display) -> Self {
        Self::Pow {
            message: error.to_string(),
        }
    }

    pub(crate) fn encode_envelope(error: impl core::fmt::Display) -> Self {
        Self::EncodeEnvelope {
            message: error.to_string(),
        }
    }

    pub(crate) fn parse_envelope(error: impl core::fmt::Display) -> Self {
        Self::ParseEnvelope {
            message: error.to_string(),
        }
    }
}
