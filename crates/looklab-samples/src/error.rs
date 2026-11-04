// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rejection reasons for [`crate::decode`], [`crate::decode_patch_table`] and
//! [`crate::SampleSet::new`].

use crate::types::Field;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("input is {len} bytes, too short for the container prefix")]
    TooShort { len: usize },

    #[error("bad magic {found:02x?}, expected {expected:02x?}")]
    BadMagic { found: [u8; 6], expected: [u8; 6] },

    #[error("unsupported blob version {0}")]
    UnsupportedVersion(u16),

    #[error("header length {len} exceeds the {max}-byte limit")]
    HeaderTooLong { len: u32, max: u32 },

    #[error("header of {len} bytes runs past the end of the {available}-byte input")]
    HeaderBeyondInput { len: usize, available: usize },

    #[error("header codec error")]
    HeaderCodec(#[source] postcard::Error),

    #[error("unexpected bytes after the postcard header")]
    TrailingHeaderBytes,

    #[error("unknown input space {0}")]
    UnknownInputSpace(u32),

    #[error("unknown output space {0}")]
    UnknownOutputSpace(u32),

    #[error("{kind} count {n} outside 1..={max}")]
    CountOutOfRange { kind: BlobKind, n: u32, max: u32 },

    #[error("{field} has {len} entries, expected {n}")]
    ColumnLengthMismatch { field: Field, len: usize, n: usize },

    #[error("zstd frame header is invalid")]
    InvalidFrameHeader,

    #[error("zstd window of {window} bytes exceeds the {max}-byte limit")]
    WindowTooLarge { window: u64, max: u64 },

    #[error("zstd frame declares a content size of {declared}, expected {expected}")]
    FrameContentSizeMismatch { declared: u64, expected: u64 },

    #[error("zstd body is invalid")]
    InvalidBody,

    #[error("unexpected bytes after the zstd frame")]
    TrailingBodyBytes,

    #[error("body decompressed to {actual} bytes, expected {expected}")]
    BodySizeMismatch { actual: usize, expected: usize },

    #[error("{field} value at index {index} is not finite")]
    NonFinite { field: Field, index: usize },

    #[error("{field} value {value} at index {index} is outside [{min}, {max}]")]
    ValueOutOfRange {
        field: Field,
        index: usize,
        value: f32,
        min: f32,
        max: f32,
    },

    #[error("sample flags {bits:#04x} at index {index} use undefined bits")]
    UnknownFlagBits { index: usize, bits: u8 },

    #[error("header field {field} is not finite")]
    NonFiniteHeader { field: &'static str },

    #[error("header field {field} = {value} is outside [{min}, {max}]")]
    HeaderValueOutOfRange {
        field: &'static str,
        value: f32,
        min: f32,
        max: f32,
    },

    #[error("patch id {id} follows {previous}, ids must be strictly ascending")]
    PatchIdsNotAscending { id: u16, previous: u16 },
}

/// Which blob a count limit applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    Samples,
    PatchRows,
}

impl core::fmt::Display for BlobKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Samples => "sample",
            Self::PatchRows => "patch row",
        })
    }
}
