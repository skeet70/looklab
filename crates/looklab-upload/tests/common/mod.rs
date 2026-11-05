// SPDX-License-Identifier: AGPL-3.0-or-later
#![allow(dead_code)]

use looklab_samples::{
    InputSpace, OutputSpace, PatchId, PatchRow, PatchTable, PatchTableHeader, PrepParamsHash,
    PrepVersion, PriorTransformVersion, RerenderGroupId, SampleFlags, SampleHeader, SampleSet,
};
use looklab_upload::{
    BatchManifest, BlobKind, BodyId, CommunitySnapshotVersion, ContributionKind, ControlEntry,
    ControlKey, ControlSettings, EnvelopeParts, FirmwareVersionString, Intent, Limits,
    ManifestImage, ManifestMetadata, Nonce, PowSolutions, RawSerial, SimKey, StepIndex,
    WebAuthnAssertion, encode_envelope,
};
use sha2::{Digest, Sha256};

pub fn metadata() -> ManifestMetadata {
    ManifestMetadata {
        make: Some("Fujifilm".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some("2.10".to_owned()),
        lens_model: Some("XF 23mm F1.4".to_owned()),
        focal_length_mm: Some(23.0),
        iso: Some(400),
        shutter_seconds: Some(0.008),
        aperture_f_number: Some(1.4),
        exposure_compensation_ev: Some(-0.33),
        as_shot_wb: None,
        capture_month: None,
        software: Some("X-T5 Ver.2.10".to_owned()),
        render_tag: None,
        render_tag_malformed: false,
        look: None,
    }
}

pub fn sample_header(rerender_group: Option<RerenderGroupId>) -> SampleHeader {
    SampleHeader {
        n: 1,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 5000.0,
        exposure_hint_ev: 0.0,
        rerender_group,
        prep_version: PrepVersion::new(1),
        prior_transform_version: PriorTransformVersion::new(1),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    }
}

pub fn sample_blob() -> Vec<u8> {
    let set = SampleSet::new(
        sample_header(Some(RerenderGroupId::new([7; 16]))),
        vec![[0.1, 0.2, 0.3]],
        vec![[0.4, 0.5, 0.6]],
        vec![1.0],
        vec![SampleFlags::NONE],
    )
    .expect("fixture samples are in range");
    looklab_samples::encode(&set).expect("fixture samples encode")
}

pub fn patch_blob() -> Vec<u8> {
    let header = PatchTableHeader {
        n: 1,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        prep_version: PrepVersion::new(1),
    };
    let row = PatchRow {
        id: PatchId::new(2),
        in_mean: [0.1, 0.2, 0.3],
        in_std: [0.01, 0.02, 0.03],
        out_mean: [0.4, 0.5, 0.6],
        out_std: [0.02, 0.02, 0.02],
    };
    let table = PatchTable::new(header, vec![row]).expect("fixture patch row is in range");
    looklab_samples::encode_patch_table(&table).expect("fixture patch table encodes")
}

pub fn image(blob: &[u8], kind: BlobKind) -> ManifestImage {
    ManifestImage {
        meta: metadata(),
        rerender_group: None,
        blob_sha256: Sha256::digest(blob).into(),
        blob_kind: kind,
    }
}

pub fn manifest(images: Vec<ManifestImage>) -> BatchManifest {
    BatchManifest {
        kind: ContributionKind::Library,
        intent: Intent::Native,
        body: BodyId::new("fujifilm/x-t5").expect("fixture body id"),
        firmware_version: FirmwareVersionString::new("2.10").expect("fixture firmware"),
        sim_key: SimKey::new("classic_chrome").expect("fixture sim key"),
        community_snapshot_version: CommunitySnapshotVersion::SHIPPED,
        settings: ControlSettings::new(vec![
            ControlEntry::reproduced(
                ControlKey::new("highlight_tone").expect("fixture control key"),
                StepIndex::new(2),
            ),
            ControlEntry::reproduced(
                ControlKey::new("shadow_tone").expect("fixture control key"),
                StepIndex::new(-1),
            ),
        ])
        .expect("fixture settings are unique"),
        images,
    }
}

pub fn assertion() -> WebAuthnAssertion {
    WebAuthnAssertion {
        credential_id: vec![0xaa; 16],
        authenticator_data: vec![0xbb; 37],
        client_data_json: br#"{"type":"webauthn.get"}"#.to_vec(),
        signature: vec![0xcc; 64],
    }
}

pub struct Fixture {
    pub nonce: Nonce,
    pub manifest: BatchManifest,
    pub assertion: WebAuthnAssertion,
    pub pow: PowSolutions,
    pub serial: Option<RawSerial>,
    pub blobs: Vec<Vec<u8>>,
}

impl Fixture {
    pub fn new() -> Self {
        let blobs = vec![sample_blob(), patch_blob()];
        let images = vec![
            image(&blobs[0], BlobKind::Samples),
            image(&blobs[1], BlobKind::PatchTable),
        ];
        Self {
            nonce: Nonce::from_bytes([3; 32]),
            manifest: manifest(images),
            assertion: assertion(),
            pow: PowSolutions::new([9; 16]),
            serial: None,
            blobs,
        }
    }

    pub fn parts(&self) -> EnvelopeParts<'_> {
        EnvelopeParts {
            nonce: self.nonce,
            manifest: &self.manifest,
            assertion: &self.assertion,
            pow: &self.pow,
            serial: self.serial.as_ref(),
            blobs: &self.blobs,
        }
    }

    pub fn bytes(&self) -> Vec<u8> {
        encode_envelope(&self.parts())
    }

    pub fn offsets(&self) -> Offsets {
        let manifest_len = looklab_upload::canonical_manifest_bytes(&self.manifest).len();
        let mut cursor = 44 + manifest_len;
        let credential_len = cursor;
        cursor += 1;
        let credential = cursor;
        cursor += self.assertion.credential_id.len();
        let authenticator_len = cursor;
        cursor += 2;
        cursor += self.assertion.authenticator_data.len();
        let client_len = cursor;
        cursor += 2;
        cursor += self.assertion.client_data_json.len();
        let signature_len = cursor;
        cursor += 2;
        cursor += self.assertion.signature.len();
        let pow_k = cursor;
        cursor += 1 + 16 * 8;
        let serial_tag = cursor;
        cursor += 1;
        if let Some(serial) = &self.serial {
            cursor += 2 + serial_envelope_len(serial);
        }
        Offsets {
            version: 6,
            nonce: 8,
            manifest_len: 40,
            manifest: 44,
            credential_len,
            credential,
            authenticator_len,
            client_len,
            signature_len,
            pow_k,
            serial_tag,
            blob_count: cursor,
        }
    }
}

fn serial_envelope_len(serial: &RawSerial) -> usize {
    let mut out = Vec::new();
    serial.write_envelope_field(&mut out);
    out.len()
}

impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Offsets {
    pub version: usize,
    pub nonce: usize,
    pub manifest_len: usize,
    pub manifest: usize,
    pub credential_len: usize,
    pub credential: usize,
    pub authenticator_len: usize,
    pub client_len: usize,
    pub signature_len: usize,
    pub pow_k: usize,
    pub serial_tag: usize,
    pub blob_count: usize,
}

/// Byte offset of the `images` length varint inside the canonical manifest.
pub fn images_count_offset(manifest: &BatchManifest) -> usize {
    let parts = [
        postcard::to_allocvec(&manifest.kind).expect("kind serializes"),
        postcard::to_allocvec(&manifest.intent).expect("intent serializes"),
        postcard::to_allocvec(&manifest.body).expect("body serializes"),
        postcard::to_allocvec(&manifest.firmware_version).expect("firmware serializes"),
        postcard::to_allocvec(&manifest.sim_key).expect("sim key serializes"),
        postcard::to_allocvec(&manifest.community_snapshot_version)
            .expect("snapshot version serializes"),
        postcard::to_allocvec(&manifest.settings).expect("settings serialize"),
    ];
    parts.iter().map(Vec::len).sum()
}

pub fn compact_limits() -> Limits {
    Limits {
        max_body_bytes: 1 << 20,
        max_manifest_bytes: 1 << 16,
        max_images: 8,
        max_blob_bytes: 1 << 16,
    }
}
