// SPDX-License-Identifier: AGPL-3.0-or-later
//! Canonical upload envelope, batch manifest, WebAuthn structure checks and
//! Argon2id proof-of-work.
//!
//! The same code produces the envelope in `looklab-wasm`, parses it strictly in
//! `looklab-server`, and reproduces it in `looklab-cli verify-payload`
//! ([docs/03 §2.3](../../docs/03-data-model.md),
//! [docs/05 §9](../../docs/05-trust-and-integrity.md),
//! [docs/06 §5](../../docs/06-privacy-and-payload.md)).

mod envelope;
pub mod ids;
mod manifest;
mod metadata;
mod nonce;
mod pow;
pub mod webauthn;

pub use envelope::{
    DEFAULT_MAX_BLOB_BYTES, DEFAULT_MAX_BODY_BYTES, DEFAULT_MAX_IMAGES, DEFAULT_MAX_MANIFEST_BYTES,
    Envelope, EnvelopeError, EnvelopeParts, Limits, MAGIC, ManifestField, VERSION, encode_envelope,
    parse_envelope_strict, peek_nonce,
};
pub use ids::{
    BodyId, CardDeclaration, ContributionKind, ControlEntry, ControlKey, ControlSettings,
    FirmwareVersionString, IdError, Intent, LookSlug, SettingsError, SimKey, StepIndex,
};
pub use looklab_meta::{
    FirmwareId, FirmwareVersion, RawSerial, RenderTag, RenderTagError, SerialHmac, SerialHmacKey,
    parse_render_tag,
};
pub use looklab_samples::RerenderGroupId;
pub use manifest::{
    BatchManifest, BlobKind, IntentError, ManifestImage, canonical_manifest_bytes, manifest_hash,
};
pub use metadata::{
    BadMonth, CaptureMonth, DynamicRange, DynamicRangeSetting, EffectStrength, FilmMode,
    GrainRoughness, GrainSize, LookSettings, ManifestMetadata, MetadataError, MetadataField,
    MonochromeFilter, NoiseReduction, Saturation, Sharpness, Tagged, WhiteBalanceFineTune,
    WhiteBalanceMode, WhiteBalanceMultipliers, WhiteBalanceShift,
};
pub use nonce::{Challenge, ManifestHash, Nonce};
pub use pow::{POW_K, PowError, PowParams, PowSolutions, solve, verify};
pub use webauthn::{
    ClientDataExtra, ExpectedAssertion, ExtensionEntry, Extensions, ReviewValue,
    SignatureAlgorithm, StructuralError, StructuralVerdict, TopOriginReview, WebAuthnAssertion,
    check_structure,
};
