// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_color::linalg::Mat3;
use looklab_prep::PrepParams;
use looklab_upload::{
    AttestationHash, BatchManifest, BlobKind, BodyId, Challenge, CommunitySnapshotVersion,
    ContributionKind, ControlSettings, FirmwareVersionString, Intent, Limits, ManifestHash,
    ManifestMetadata, MonochromeAuthorization, Nonce, POW_K, RenderClass, SendManifest,
    SendManifestHash, SimKey,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PriorTransformSpec {
    /// A ready camera-to-XYZ matrix; `cct_k` zero means unknown.
    Matrix {
        xyz_from_camera: Mat3,
        cct_k: f64,
        version: u32,
    },
    /// Derive the prior from the RAW's own colour matrices and as-shot white balance.
    CameraMatrices { version: u32 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrepareRequest {
    pub raw: Vec<u8>,
    pub jpegs: Vec<Vec<u8>>,
    pub prior: PriorTransformSpec,
    pub seed: [u8; 32],
    pub params: PrepParams,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AlignmentSummary {
    pub correlation: f64,
    /// Median block shift in original RAW pixels.
    pub residual_px: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparedImageView {
    pub blob: Vec<u8>,
    pub metadata: ManifestMetadata,
    pub removed: Vec<String>,
    pub rerender_group: Option<[u8; 16]>,
    pub alignment: AlignmentSummary,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PrepareResponse {
    pub images: Vec<PreparedImageView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ManifestImageInput {
    pub metadata: ManifestMetadata,
    pub rerender_group: Option<[u8; 16]>,
    pub blob: Vec<u8>,
    pub kind: BlobKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildManifestRequest {
    pub kind: ContributionKind,
    pub intent: Intent,
    pub render_class: RenderClass,
    pub monochrome_authorization: Option<MonochromeAuthorization>,
    pub body: BodyId,
    pub firmware: FirmwareVersionString,
    pub sim: SimKey,
    pub community_snapshot_version: CommunitySnapshotVersion,
    pub settings: ControlSettings,
    pub images: Vec<ManifestImageInput>,
    pub nonce: Nonce,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildManifestResponse {
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: ManifestHash,
    pub challenge: Challenge,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BuildSendManifestRequest {
    pub nonce: Nonce,
    pub manifest_hashes: Vec<ManifestHash>,
    #[serde(default)]
    pub attestation_hashes: Vec<AttestationHash>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildSendManifestResponse {
    pub send_manifest_bytes: Vec<u8>,
    pub send_manifest_hash: SendManifestHash,
    pub challenge: Challenge,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssertionFields {
    pub credential_id: Vec<u8>,
    pub authenticator_data: Vec<u8>,
    pub client_data_json: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EncodeEnvelopeRequest {
    pub nonce: Nonce,
    pub send_manifest_bytes: Vec<u8>,
    pub manifest_bytes: Vec<u8>,
    pub assertion: AssertionFields,
    pub pow: [u64; POW_K],
    pub serial: Option<Vec<u8>>,
    pub blobs: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeEnvelopeResponse {
    pub envelope: Vec<u8>,
    pub manifest_hash: ManifestHash,
    pub send_manifest_hash: SendManifestHash,
    pub challenge: Challenge,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct PowParamsSpec {
    pub memory_kib: u32,
    pub iterations: u32,
    pub difficulty_bits_per_image: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SolvePowRequest {
    pub challenge: Challenge,
    pub n_images: u32,
    pub params: PowParamsSpec,
    pub start: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SolvePowResponse {
    pub solutions: [u64; POW_K],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParseEnvelopeRequest {
    pub bytes: Vec<u8>,
    pub limits: Limits,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobSummary {
    pub index: u32,
    pub len: u32,
    pub sha256: [u8; 32],
    pub kind: BlobKind,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ParseEnvelopeResponse {
    pub body_bytes: u64,
    pub nonce: Nonce,
    pub send_manifest: SendManifest,
    pub send_manifest_bytes: Vec<u8>,
    pub send_manifest_hash: SendManifestHash,
    pub manifest: BatchManifest,
    pub manifest_bytes: Vec<u8>,
    pub manifest_hash: ManifestHash,
    pub challenge: Challenge,
    pub assertion: AssertionFields,
    pub pow: [u64; POW_K],
    pub serial_present: bool,
    pub blobs: Vec<BlobSummary>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionRequest;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionResponse {
    pub module_abi_version: u32,
    pub prep_version: u32,
    pub crate_version: String,
    pub build_id: [u8; 32],
}

impl VersionResponse {
    /// Whether this reply describes the module this build produces.
    pub fn matches_current(&self) -> bool {
        self.module_abi_version == crate::MODULE_ABI_VERSION
            && self.prep_version == crate::PREP_VERSION
            && self.build_id == crate::build_id()
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Response {
    Version(VersionResponse),
    Prepare(PrepareResponse),
    BuildManifest(BuildManifestResponse),
    BuildSendManifest(BuildSendManifestResponse),
    EncodeEnvelope(EncodeEnvelopeResponse),
    SolvePow(SolvePowResponse),
    ParseEnvelope(Box<ParseEnvelopeResponse>),
}
