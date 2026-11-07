// SPDX-License-Identifier: AGPL-3.0-or-later
//! Postcard requests for the prep module ([docs/03 §2.3](../../../docs/03-data-model.md)).

use looklab_meta::RawSerial;
use looklab_prep::PrepParams;
use looklab_prep_module::{
    AssertionFields, BuildManifestRequest, BuildManifestResponse, EncodeEnvelopeRequest,
    EncodeEnvelopeResponse, Function, ManifestImageInput, ModuleError, ParseEnvelopeRequest,
    ParseEnvelopeResponse, PowParamsSpec, PrepareRequest, PrepareResponse, PreparedImageView,
    PriorTransformSpec, Response, SolvePowRequest, SolvePowResponse,
};
use looklab_upload::{
    BlobKind, BodyId, CardDeclaration, Challenge, CommunitySnapshotVersion, ContributionKind,
    ControlEntry, ControlKey, ControlSettings, Intent, Limits, LookSlug, ManifestHash, Nonce,
    SettingSource, SimKey, StepIndex,
};

use crate::error::FacadeError;

/// One control in a batch's settings, in any order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlSetting {
    pub key: String,
    pub step: i64,
    pub source: SettingSource,
}

/// Whole-group settings shared by every image of one batch.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupSettings {
    pub kind: ContributionKind,
    pub intent: IntentChoice,
    pub body: String,
    pub firmware: String,
    pub sim: String,
    pub community_snapshot_version: u64,
    pub controls: Vec<ControlSetting>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IntentChoice {
    Native,
    Recipe(String),
    Card(CardDeclaration),
}

impl IntentChoice {
    fn into_intent(self) -> Result<Intent, FacadeError> {
        match self {
            Self::Native => Ok(Intent::Native),
            Self::Recipe(slug) => Ok(Intent::Recipe {
                target_look: LookSlug::new(&slug)
                    .map_err(|error| FacadeError::Id(error.to_string()))?,
            }),
            Self::Card(declaration) => Ok(Intent::Card(declaration)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PrepSummary {
    pub metadata: looklab_upload::ManifestMetadata,
    pub removed: Vec<String>,
    pub rerender_group: Option<[u8; 16]>,
    pub alignment: looklab_prep_module::AlignmentSummary,
    pub blob_len: usize,
}

pub fn summarize(images: &[PreparedImageView]) -> Vec<PrepSummary> {
    images
        .iter()
        .map(|image| PrepSummary {
            metadata: image.metadata.clone(),
            removed: image.removed.clone(),
            rerender_group: image.rerender_group,
            alignment: image.alignment,
            blob_len: image.blob.len(),
        })
        .collect()
}

pub fn default_pow() -> PowParamsSpec {
    let params = looklab_upload::PowParams::default();
    PowParamsSpec {
        memory_kib: params.memory_kib,
        iterations: params.iterations,
        difficulty_bits_per_image: params.difficulty_bits_per_image,
    }
}

pub fn module_fn_id(function: ModuleFunction) -> u32 {
    function.function().id()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModuleFunction {
    Version,
    Prepare,
    BuildManifest,
    EncodeEnvelope,
    SolvePow,
    ParseEnvelope,
}

impl ModuleFunction {
    const fn function(self) -> Function {
        match self {
            Self::Version => Function::Version,
            Self::Prepare => Function::Prepare,
            Self::BuildManifest => Function::BuildManifest,
            Self::EncodeEnvelope => Function::EncodeEnvelope,
            Self::SolvePow => Function::SolvePow,
            Self::ParseEnvelope => Function::ParseEnvelope,
        }
    }
}

pub fn version_request() -> Vec<u8> {
    postcard::to_allocvec(&looklab_prep_module::VersionRequest).expect("unit request is encodable")
}

pub fn prepare_request(
    raw: Vec<u8>,
    jpegs: Vec<Vec<u8>>,
    prior_version: u32,
    seed: [u8; 32],
) -> Vec<u8> {
    let request = PrepareRequest {
        raw,
        jpegs,
        prior: PriorTransformSpec::CameraMatrices {
            version: prior_version,
        },
        seed,
        params: PrepParams::default(),
    };
    postcard::to_allocvec(&request).expect("prepare request is encodable")
}

/// Builds the manifest for the images the module prepared in this batch.
///
/// # Errors
/// Rejects an identifier, a settings list or a nonce that is not canonical.
pub fn build_manifest_request(
    settings: &GroupSettings,
    images: &[PreparedImageView],
    nonce: [u8; 32],
) -> Result<Vec<u8>, FacadeError> {
    let entries = settings
        .controls
        .iter()
        .map(|control| {
            Ok(ControlEntry::new(
                ControlKey::new(&control.key)
                    .map_err(|error| FacadeError::Id(error.to_string()))?,
                StepIndex::new(control.step),
                control.source,
            ))
        })
        .collect::<Result<Vec<_>, FacadeError>>()?;
    let request = BuildManifestRequest {
        kind: settings.kind,
        intent: settings.intent.clone().into_intent()?,
        body: BodyId::new(&settings.body).map_err(|error| FacadeError::Id(error.to_string()))?,
        firmware: looklab_upload::FirmwareVersionString::new(&settings.firmware)
            .map_err(|error| FacadeError::Id(error.to_string()))?,
        sim: SimKey::new(&settings.sim).map_err(|error| FacadeError::Id(error.to_string()))?,
        community_snapshot_version: CommunitySnapshotVersion::new(
            settings.community_snapshot_version,
        ),
        settings: ControlSettings::new(entries)?,
        images: images
            .iter()
            .map(|image| ManifestImageInput {
                metadata: image.metadata.clone(),
                rerender_group: image.rerender_group,
                blob: image.blob.clone(),
                kind: BlobKind::Samples,
            })
            .collect(),
        nonce: Nonce::from_slice(&nonce)
            .ok_or_else(|| FacadeError::Input("nonce is not 32 bytes".to_owned()))?,
    };
    Ok(postcard::to_allocvec(&request).expect("manifest request is encodable"))
}

pub fn encode_envelope_request(
    nonce: [u8; 32],
    manifest_bytes: Vec<u8>,
    assertion: AssertionFields,
    pow: [u64; looklab_upload::POW_K],
    serial: Option<Vec<u8>>,
    blobs: Vec<Vec<u8>>,
) -> Result<Vec<u8>, FacadeError> {
    let request = EncodeEnvelopeRequest {
        nonce: Nonce::from_slice(&nonce)
            .ok_or_else(|| FacadeError::Input("nonce is not 32 bytes".to_owned()))?,
        manifest_bytes,
        assertion,
        pow,
        serial,
        blobs,
    };
    Ok(postcard::to_allocvec(&request).expect("envelope request is encodable"))
}

pub fn solve_pow_request(
    challenge: [u8; 32],
    n_images: u32,
    params: PowParamsSpec,
    start: u64,
) -> Result<Vec<u8>, FacadeError> {
    let request = SolvePowRequest {
        challenge: Challenge::from_slice(&challenge)
            .ok_or_else(|| FacadeError::Input("challenge is not 32 bytes".to_owned()))?,
        n_images,
        params,
        start,
    };
    Ok(postcard::to_allocvec(&request).expect("pow request is encodable"))
}

pub fn parse_envelope_request(bytes: Vec<u8>, limits: Limits) -> Vec<u8> {
    let request = ParseEnvelopeRequest { bytes, limits };
    postcard::to_allocvec(&request).expect("parse request is encodable")
}

pub fn decode_reply(bytes: &[u8]) -> Result<Response, FacadeError> {
    let (reply, rest) = postcard::take_from_bytes::<Result<Response, ModuleError>>(bytes)
        .map_err(|error| FacadeError::Reply(error.to_string()))?;
    if !rest.is_empty() {
        return Err(FacadeError::Reply(format!(
            "{} trailing bytes after the module reply",
            rest.len()
        )));
    }
    reply.map_err(FacadeError::Module)
}

pub fn decode_prepare_reply(bytes: &[u8]) -> Result<PrepareResponse, FacadeError> {
    match decode_reply(bytes)? {
        Response::Prepare(response) => Ok(response),
        other => Err(FacadeError::Reply(format!(
            "prepare reply came back as {}",
            response_name(&other)
        ))),
    }
}

pub fn decode_build_manifest_reply(bytes: &[u8]) -> Result<BuildManifestResponse, FacadeError> {
    match decode_reply(bytes)? {
        Response::BuildManifest(response) => Ok(response),
        other => Err(FacadeError::Reply(format!(
            "manifest reply came back as {}",
            response_name(&other)
        ))),
    }
}

pub fn decode_encode_reply(bytes: &[u8]) -> Result<EncodeEnvelopeResponse, FacadeError> {
    match decode_reply(bytes)? {
        Response::EncodeEnvelope(response) => Ok(response),
        other => Err(FacadeError::Reply(format!(
            "envelope reply came back as {}",
            response_name(&other)
        ))),
    }
}

pub fn decode_pow_reply(bytes: &[u8]) -> Result<SolvePowResponse, FacadeError> {
    match decode_reply(bytes)? {
        Response::SolvePow(response) => Ok(response),
        other => Err(FacadeError::Reply(format!(
            "pow reply came back as {}",
            response_name(&other)
        ))),
    }
}

pub fn decode_parse_reply(bytes: &[u8]) -> Result<ParseEnvelopeResponse, FacadeError> {
    match decode_reply(bytes)? {
        Response::ParseEnvelope(response) => Ok(*response),
        other => Err(FacadeError::Reply(format!(
            "parse reply came back as {}",
            response_name(&other)
        ))),
    }
}

fn response_name(response: &Response) -> &'static str {
    match response {
        Response::Version(_) => "version",
        Response::Prepare(_) => "prepare",
        Response::BuildManifest(_) => "manifest",
        Response::EncodeEnvelope(_) => "envelope",
        Response::SolvePow(_) => "pow",
        Response::ParseEnvelope(_) => "parse",
    }
}

/// The serial bytes the module's encode request carries.
pub fn serial_ascii(serial: &RawSerial) -> Vec<u8> {
    let mut field = Vec::new();
    serial.write_envelope_field(&mut field);
    field.get(2..).map(<[u8]>::to_vec).unwrap_or_default()
}

pub fn manifest_hash(bytes: &[u8]) -> Option<ManifestHash> {
    ManifestHash::from_slice(bytes)
}

/// The EXIF serial of `bytes`, for the envelope only.
pub fn serial_ascii_for_file(bytes: &[u8]) -> Option<Vec<u8>> {
    let metadata = looklab_meta::parse(bytes).ok()?;
    serial_ascii_from_metadata(&metadata)
}

/// For the envelope only.
pub fn serial_ascii_from_metadata(metadata: &looklab_meta::ParsedMetadata) -> Option<Vec<u8>> {
    metadata.body_serial().map(serial_ascii)
}
