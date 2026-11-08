// SPDX-License-Identifier: AGPL-3.0-or-later
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PrepError {
    #[error("JPEG decoding failed: {0}")]
    DecodeJpeg(String),
    #[error("JPEG structure is invalid: {0}")]
    JpegStructure(&'static str),
    #[error("image has no pixels")]
    ImageEmpty,
    #[error("image dimensions overflow")]
    ImageTooLarge,
    #[error("aligned overlap covers less than the required fraction")]
    AlignNoOverlap,
    #[error("central correlation {correlation:.4} is below the {minimum:.2} threshold")]
    AlignLowCorrelation { correlation: f64, minimum: f64 },
    #[error("re-render {index} does not align with the RAW: {message}")]
    MemberAlignment { index: usize, message: String },
    #[error("RAW decoding failed: {0}")]
    RawDecode(#[from] looklab_raw::RawError),
    #[error("metadata parsing failed: {0}")]
    Meta(#[from] looklab_meta::MetaError),
    #[error("sample set is invalid: {0}")]
    SampleSet(#[from] looklab_samples::Error),
    #[error("no pixel passed the masks and the density sampling")]
    NoSamples,
    #[error("invalid preparation parameter `{field}`")]
    InvalidParams { field: &'static str },
    #[error("as-shot white balance multipliers are outside the accepted range")]
    InvalidWbMultipliers,
    #[error("camera colour matrices do not yield a prior transform")]
    PriorTransform,
    #[error("could not encode the preparation parameters")]
    ParamsPostcard(#[from] postcard::Error),
    #[error("a re-render group needs at least one JPEG")]
    NoJpeg,
    #[error("an aligned sample falls outside the JPEG")]
    SampleOutsideJpeg,
    #[error("the chart region was measured on a different JPEG size")]
    ChartRegionSize,
    #[error("a sampled value is not finite")]
    NonFiniteSample,
    #[error("operating-system randomness is unavailable")]
    SeedUnavailable,
    #[error("seed file JSON error: {0}")]
    SeedFileJson(#[from] serde_json::Error),
    #[error("seed file format string is not \"looklab-seeds/1\"")]
    SeedFileFormat,
    #[error("seed file entry {index} is invalid")]
    SeedFileEntry { index: usize },
}
