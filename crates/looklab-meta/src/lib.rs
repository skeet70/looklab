// SPDX-License-Identifier: AGPL-3.0-or-later
//! EXIF and makernote parsing, film-simulation extraction, whitelist sanitizer.

mod attest;
mod container;
mod error;
mod exif;
mod fuji;
mod render_tag;
mod sanitize;
mod serial;
mod tiff;
mod types;

pub use attest::{PairingData, has_makernotes, pairing_data, software_matches_firmware};
pub use container::{parse, parse_jpeg, parse_raf, parse_tiff};
pub use error::MetaError;
pub use exif::{ExifData, ImageUniqueId, ParsedMetadata};
pub use fuji::{
    Clarity, ColorSaturation, DevelopmentDynamicRange, DynamicRange, DynamicRangeSetting,
    EffectStrength, FilmMode, FujiMakernote, FujiTag, GrainRoughness, GrainSize, ImageGeneration,
    LookSettings, MonochromeFilter, NoiseReduction, ParsedPair, ParsedTag, Sharpness,
    ToneAdjustment, WhiteBalanceFineTune, WhiteBalanceMode,
};
pub use render_tag::{
    CONTENT_ID_PREFIX_LEN, FirmwareId, FirmwareVersion, MAX_FIRMWARE_ID_LEN,
    MAX_FIRMWARE_VERSION_LEN, MAX_LOOK_SLUG_LEN, MAX_LOOK_VERSION_LEN, RENDER_TAG_PREFIX,
    RenderLookSlug, RenderLookVersion, RenderTag, RenderTagError, parse_render_tag,
};
pub use sanitize::{RemovedField, RemovedFields, SanitizedMetadata, sanitize};
pub use serial::{RawSerial, SerialHmac, SerialHmacKey};
pub use types::{
    Aperture, CaptureTime, ExposureCompensation, ExposureTime, FocalLength, ImageNumber, Iso,
    Orientation, SubSec, WhiteBalanceMultipliers, YearMonth,
};
