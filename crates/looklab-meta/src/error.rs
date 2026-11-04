// SPDX-License-Identifier: AGPL-3.0-or-later
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum MetaError {
    #[error("file is not a JPEG")]
    NotJpeg,
    #[error("file is not a Fujifilm RAF")]
    NotRaf,
    #[error("file is not a TIFF or DNG")]
    NotTiff,
    #[error("file is not a supported container")]
    UnknownContainer,
    #[error("no EXIF APP1 segment")]
    NoExif,
    #[error("truncated metadata")]
    Truncated,
    #[error("invalid TIFF structure")]
    InvalidTiff,
    #[error("invalid RAF header")]
    InvalidRaf,
    #[error("invalid serial number")]
    InvalidSerial,
}
