// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_meta::RawSerial;
use looklab_prep::{
    PrepParams, PriorTransform, SamplingSeed, SeedFile, prepare_group_with_decoded_raw,
};
use looklab_raw::{DecodedRaw, RawDecoder, RawlerDecoder};
use looklab_samples::{PriorTransformVersion, encode};
use looklab_upload::webauthn::{
    MAX_AUTHENTICATOR_DATA, MAX_CLIENT_DATA_JSON, MAX_CREDENTIAL_ID, MAX_SIGNATURE,
    WebAuthnAssertion,
};
use looklab_upload::{
    Challenge, DEFAULT_MAX_BATCHES, Envelope, EnvelopeParts, ManifestImage, PowParams,
    PowSolutions, RerenderGroupId, SendManifest, canonical_manifest_bytes,
    canonical_send_manifest_bytes, encode_envelope as encode_upload_envelope, manifest_hash,
    parse_envelope_strict, send_manifest_hash, solve,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::error::ModuleError;
use crate::types::{
    AlignmentSummary, AssertionFields, BlobSummary, BuildManifestRequest, BuildManifestResponse,
    BuildSendManifestRequest, BuildSendManifestResponse, EncodeEnvelopeRequest,
    EncodeEnvelopeResponse, ParseEnvelopeRequest, ParseEnvelopeResponse, PrepareResponse,
    PreparedImageView, PriorTransformSpec, SolvePowRequest, SolvePowResponse,
};

const SEED_KEY: &str = "module-input";

#[derive(Deserialize)]
pub(crate) struct PrepareInput<'a> {
    #[serde(borrow)]
    raw: &'a [u8],
    #[serde(borrow)]
    jpegs: Vec<&'a [u8]>,
    prior: PriorTransformSpec,
    seed: [u8; 32],
    params: PrepParams,
}

pub(crate) fn prepare(request: PrepareInput<'_>) -> Result<PrepareResponse, ModuleError> {
    if request.jpegs.is_empty() {
        return Err(ModuleError::prep("a group needs at least one JPEG"));
    }
    let decoded = RawlerDecoder
        .decode(request.raw)
        .map_err(ModuleError::prep)?;
    let prior = prior_transform(&request.prior, &decoded)?;
    let seed = sampling_seed(request.seed, prior.version())?;
    let (prepared, alignment) = prepare_group_with_decoded_raw(
        request.raw,
        decoded,
        &request.jpegs,
        &prior,
        seed,
        &request.params,
    )
    .map_err(ModuleError::prep)?;
    let alignment = AlignmentSummary {
        correlation: alignment.central_correlation,
        residual_px: alignment.residual_px,
    };

    let mut images = Vec::with_capacity(prepared.len());
    for image in prepared {
        images.push(PreparedImageView {
            blob: encode(&image.samples).map_err(ModuleError::prep)?,
            metadata: (&image.sanitized).into(),
            removed: image
                .removed
                .iter()
                .map(|field| field.label().to_owned())
                .collect(),
            rerender_group: image
                .samples
                .header()
                .rerender_group
                .map(|group| *group.as_bytes()),
            alignment,
        });
    }
    Ok(PrepareResponse { images })
}

fn prior_transform(
    spec: &PriorTransformSpec,
    decoded: &DecodedRaw,
) -> Result<PriorTransform, ModuleError> {
    match *spec {
        PriorTransformSpec::Matrix {
            xyz_from_camera,
            cct_k,
            version,
        } => Ok(PriorTransform::new(
            xyz_from_camera,
            cct_k,
            PriorTransformVersion::new(version),
        )),
        PriorTransformSpec::CameraMatrices { version } => {
            let matrices = decoded
                .color_matrices
                .as_ref()
                .ok_or(ModuleError::NoColorMatrices)?;
            PriorTransform::from_camera_matrices(
                matrices,
                decoded.as_shot_wb,
                PriorTransformVersion::new(version),
            )
            .map_err(ModuleError::prep)
        }
    }
}

/// Prep exports seeds only through the local seed-file channel.
fn sampling_seed(
    bytes: [u8; 32],
    version: PriorTransformVersion,
) -> Result<SamplingSeed, ModuleError> {
    let key = looklab_prep::LocalImageKey::new(SEED_KEY)
        .ok_or_else(|| ModuleError::prep("seed key is invalid"))?;
    let json = format!(
        "{{\"format\":\"looklab-seeds/1\",\"prior_transform_version\":{},\"entries\":[{{\"key\":\"{SEED_KEY}\",\"seed\":\"{}\"}}]}}",
        version.get(),
        to_hex(&bytes)
    );
    let file = SeedFile::from_json(&json).map_err(ModuleError::prep)?;
    file.seed_for(&key)
        .ok_or_else(|| ModuleError::prep("seed file has no entry"))
}

fn to_hex(bytes: &[u8; 32]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

pub(crate) fn build_manifest(
    request: BuildManifestRequest,
) -> Result<BuildManifestResponse, ModuleError> {
    let mut images = Vec::with_capacity(request.images.len());
    for (index, image) in request.images.into_iter().enumerate() {
        image
            .metadata
            .validate()
            .map_err(|error| ModuleError::Metadata {
                index,
                message: error.to_string(),
            })?;
        images.push(ManifestImage {
            meta: image.metadata,
            rerender_group: image.rerender_group.map(RerenderGroupId::new),
            blob_sha256: sha256(&image.blob),
            blob_kind: image.kind,
        });
    }
    let manifest = looklab_upload::BatchManifest {
        kind: request.kind,
        intent: request.intent,
        body: request.body,
        firmware_version: request.firmware,
        sim_key: request.sim,
        community_snapshot_version: request.community_snapshot_version,
        settings: request.settings,
        images,
    };
    let manifest_bytes = canonical_manifest_bytes(&manifest);
    let hash = manifest_hash(&manifest);
    Ok(BuildManifestResponse {
        manifest_bytes,
        manifest_hash: hash,
        challenge: Challenge::new(&request.nonce, &hash),
    })
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

pub(crate) fn build_send_manifest(
    request: BuildSendManifestRequest,
) -> Result<BuildSendManifestResponse, ModuleError> {
    let send_manifest = SendManifest::canonical(request.manifest_hashes).map_err(|error| {
        ModuleError::SendManifest {
            message: error.to_string(),
        }
    })?;
    let hash = send_manifest_hash(&send_manifest);
    Ok(BuildSendManifestResponse {
        send_manifest_bytes: canonical_send_manifest_bytes(&send_manifest),
        send_manifest_hash: hash,
        challenge: Challenge::for_send(&request.nonce, &hash),
    })
}

pub(crate) fn encode_envelope(
    request: EncodeEnvelopeRequest,
) -> Result<EncodeEnvelopeResponse, ModuleError> {
    let send_manifest: SendManifest =
        postcard::from_bytes(&request.send_manifest_bytes).map_err(|error| {
            ModuleError::SendManifest {
                message: error.to_string(),
            }
        })?;
    if canonical_send_manifest_bytes(&send_manifest) != request.send_manifest_bytes {
        return Err(ModuleError::SendManifest {
            message: "bytes are not the canonical encoding of the send manifest".to_owned(),
        });
    }
    send_manifest
        .validate(DEFAULT_MAX_BATCHES)
        .map_err(|error| ModuleError::SendManifest {
            message: error.to_string(),
        })?;
    let manifest: looklab_upload::BatchManifest = postcard::from_bytes(&request.manifest_bytes)
        .map_err(|error| ModuleError::Manifest {
            message: error.to_string(),
        })?;
    if canonical_manifest_bytes(&manifest) != request.manifest_bytes {
        return Err(ModuleError::Manifest {
            message: "bytes are not the canonical encoding of the manifest".to_owned(),
        });
    }
    let hash = manifest_hash(&manifest);
    if !send_manifest.contains(&hash) {
        return Err(ModuleError::SendManifest {
            message: "the batch manifest is not listed in the send manifest".to_owned(),
        });
    }
    let images = manifest.images.len();
    if request.blobs.len() != images {
        return Err(ModuleError::BlobCount {
            count: request.blobs.len(),
            images,
        });
    }
    if images == 0 {
        return Err(ModuleError::Manifest {
            message: "a batch needs at least one image".to_owned(),
        });
    }
    for (index, image) in manifest.images.iter().enumerate() {
        image
            .meta
            .validate()
            .map_err(|error| ModuleError::Metadata {
                index,
                message: error.to_string(),
            })?;
    }
    check_assertion(&request.assertion)?;
    if request.blobs.len() > usize::from(u16::MAX) {
        return Err(ModuleError::EncodeEnvelope {
            message: format!("{} blobs exceed the envelope limit", request.blobs.len()),
        });
    }
    let serial = request
        .serial
        .as_deref()
        .map(RawSerial::from_envelope_ascii)
        .transpose()
        .map_err(ModuleError::encode_envelope)?;
    let assertion = WebAuthnAssertion {
        credential_id: request.assertion.credential_id,
        authenticator_data: request.assertion.authenticator_data,
        client_data_json: request.assertion.client_data_json,
        signature: request.assertion.signature,
    };
    let pow = PowSolutions::new(request.pow);
    let parts = EnvelopeParts {
        nonce: request.nonce,
        send_manifest: &send_manifest,
        manifest: &manifest,
        assertion: &assertion,
        pow: &pow,
        serial: serial.as_ref(),
        blobs: &request.blobs,
    };
    let send_hash = send_manifest_hash(&send_manifest);
    Ok(EncodeEnvelopeResponse {
        envelope: encode_upload_envelope(&parts),
        manifest_hash: hash,
        send_manifest_hash: send_hash,
        challenge: Challenge::for_send(&request.nonce, &send_hash),
    })
}

fn check_assertion(assertion: &AssertionFields) -> Result<(), ModuleError> {
    let checks = [
        (
            assertion.credential_id.is_empty(),
            "credential id is empty".to_owned(),
        ),
        (
            assertion.credential_id.len() > MAX_CREDENTIAL_ID,
            "credential id is over the envelope limit".to_owned(),
        ),
        (
            assertion.authenticator_data.len() > MAX_AUTHENTICATOR_DATA,
            "authenticator data is over the envelope limit".to_owned(),
        ),
        (
            assertion.client_data_json.len() > MAX_CLIENT_DATA_JSON,
            "client_data_json is over the envelope limit".to_owned(),
        ),
        (
            assertion.signature.len() > MAX_SIGNATURE,
            "signature is over the envelope limit".to_owned(),
        ),
    ];
    for (exceeded, message) in checks {
        if exceeded {
            return Err(ModuleError::EncodeEnvelope { message });
        }
    }
    Ok(())
}

pub(crate) fn solve_pow(request: SolvePowRequest) -> Result<SolvePowResponse, ModuleError> {
    let params = PowParams {
        memory_kib: request.params.memory_kib,
        iterations: request.params.iterations,
        difficulty_bits_per_image: request.params.difficulty_bits_per_image,
    };
    let solutions = solve(&request.challenge, request.n_images, &params, request.start)
        .map_err(ModuleError::pow)?;
    Ok(SolvePowResponse {
        solutions: *solutions.as_array(),
    })
}

pub(crate) fn parse_envelope(
    request: ParseEnvelopeRequest,
) -> Result<ParseEnvelopeResponse, ModuleError> {
    let envelope = parse_envelope_strict(&request.bytes, &request.limits)
        .map_err(ModuleError::parse_envelope)?;
    let Envelope {
        nonce,
        send_manifest,
        send_manifest_bytes,
        send_manifest_hash,
        manifest,
        manifest_bytes,
        assertion,
        pow,
        serial,
        blobs,
    } = envelope;
    let hash = manifest_hash(&manifest);
    let mut summaries = Vec::with_capacity(blobs.len());
    for (index, blob) in blobs.iter().enumerate() {
        summaries.push(BlobSummary {
            index: u32::try_from(index)
                .map_err(|_| ModuleError::parse_envelope("blob index does not fit u32"))?,
            len: u32::try_from(blob.len())
                .map_err(|_| ModuleError::parse_envelope("blob length does not fit u32"))?,
            sha256: sha256(blob),
            kind: manifest.images[index].blob_kind,
        });
    }
    Ok(ParseEnvelopeResponse {
        body_bytes: request.bytes.len() as u64,
        nonce,
        send_manifest,
        send_manifest_bytes,
        send_manifest_hash,
        manifest,
        manifest_bytes,
        manifest_hash: hash,
        challenge: Challenge::new(&nonce, &hash),
        assertion: AssertionFields {
            credential_id: assertion.credential_id,
            authenticator_data: assertion.authenticator_data,
            client_data_json: assertion.client_data_json,
            signature: assertion.signature,
        },
        pow: *pow.as_array(),
        serial_present: serial.is_some(),
        blobs: summaries,
    })
}
