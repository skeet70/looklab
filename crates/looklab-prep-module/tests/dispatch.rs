// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_prep::PREP_VERSION;
use looklab_prep_module::{
    AssertionFields, BuildManifestRequest, EncodeEnvelopeRequest, Function, ManifestImageInput,
    ModuleError, ParseEnvelopeRequest, PowParamsSpec, Response, SolvePowRequest, VersionRequest,
    VersionResponse, dispatch,
};
use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_upload::{
    BlobKind, BodyId, Challenge, CommunitySnapshotVersion, ContributionKind, ControlSettings,
    FirmwareVersionString, Intent, Limits, ManifestMetadata, Nonce, POW_K, SimKey, manifest_hash,
    verify,
};

fn call<T: serde::Serialize, U: serde::de::DeserializeOwned>(
    function: Function,
    request: &T,
    pick: impl Fn(Response) -> U,
) -> Result<U, ModuleError> {
    let input = postcard::to_allocvec(request).expect("request encodes");
    let output = dispatch(function.id(), &input);
    let result: Result<Response, ModuleError> =
        postcard::from_bytes(&output).expect("result decodes");
    result.map(pick)
}

fn metadata() -> ManifestMetadata {
    ManifestMetadata {
        make: Some("FUJIFILM".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some("2.10".to_owned()),
        lens_model: None,
        focal_length_mm: None,
        iso: None,
        shutter_seconds: None,
        aperture_f_number: None,
        exposure_compensation_ev: None,
        as_shot_wb: None,
        capture_month: None,
        software: None,
        render_tag: None,
        render_tag_malformed: false,
        look: None,
    }
}

fn sample_blob() -> Vec<u8> {
    let header = SampleHeader {
        n: 1,
        input_space: InputSpace::CameraRgbWbLinear,
        output_space: OutputSpace::Srgb,
        wb_multipliers: [1.0, 1.0, 1.0],
        cct_hint_k: 5000.0,
        exposure_hint_ev: 0.0,
        rerender_group: None,
        prep_version: PrepVersion::new(PREP_VERSION),
        prior_transform_version: PriorTransformVersion::new(3),
        prep_params_hash: PrepParamsHash::new([0; 32]),
    };
    let set = SampleSet::new(
        header,
        vec![[0.2, 0.3, 0.4]],
        vec![[0.5, 0.6, 0.7]],
        vec![1.0],
        vec![SampleFlags::NONE],
    )
    .expect("sample set is valid");
    looklab_samples::encode(&set).expect("sample set encodes")
}

fn manifest_request(nonce: Nonce, blob: Vec<u8>) -> BuildManifestRequest {
    BuildManifestRequest {
        kind: ContributionKind::Library,
        intent: Intent::Native,
        body: BodyId::new("fujifilm/x-t5").expect("body id is valid"),
        firmware: FirmwareVersionString::new("2.10").expect("firmware is valid"),
        sim: SimKey::new("classic-chrome").expect("sim key is valid"),
        community_snapshot_version: CommunitySnapshotVersion::SHIPPED,
        settings: ControlSettings::new(Vec::new()).expect("empty settings are valid"),
        images: vec![ManifestImageInput {
            metadata: metadata(),
            rerender_group: None,
            blob,
            kind: BlobKind::Samples,
        }],
        nonce,
    }
}

#[test]
fn version_reports_the_module_versions() {
    let version = call(
        Function::Version,
        &VersionRequest,
        |response| match response {
            Response::Version(version) => version,
            other => panic!("unexpected response {other:?}"),
        },
    )
    .expect("version succeeds");
    assert_eq!(
        version,
        VersionResponse {
            module_abi_version: 1,
            prep_version: PREP_VERSION,
            crate_version: env!("CARGO_PKG_VERSION").to_owned(),
            build_id: looklab_prep_module::build_id(),
        }
    );
}

#[test]
fn envelope_round_trips_through_build_encode_and_parse() {
    let nonce = Nonce::from_bytes([7; 32]);
    let request = manifest_request(nonce, sample_blob());
    let built = call(
        Function::BuildManifest,
        &request,
        |response| match response {
            Response::BuildManifest(built) => built,
            other => panic!("unexpected response {other:?}"),
        },
    )
    .expect("manifest builds");
    let manifest = postcard::from_bytes::<looklab_upload::BatchManifest>(&built.manifest_bytes)
        .expect("manifest bytes decode");
    assert_eq!(built.manifest_hash, manifest_hash(&manifest));
    assert_eq!(
        built.challenge,
        Challenge::new(&nonce, &built.manifest_hash)
    );

    let encode = EncodeEnvelopeRequest {
        nonce,
        manifest_bytes: built.manifest_bytes.clone(),
        assertion: AssertionFields {
            credential_id: vec![1],
            authenticator_data: vec![0; 37],
            client_data_json: b"{}".to_vec(),
            signature: vec![2; 64],
        },
        pow: [0; POW_K],
        serial: None,
        blobs: request
            .images
            .iter()
            .map(|image| image.blob.clone())
            .collect(),
    };
    let encoded = call(
        Function::EncodeEnvelope,
        &encode,
        |response| match response {
            Response::EncodeEnvelope(encoded) => encoded,
            other => panic!("unexpected response {other:?}"),
        },
    )
    .expect("envelope encodes");
    assert_eq!(encoded.manifest_hash, built.manifest_hash);

    let parsed = call(
        Function::ParseEnvelope,
        &ParseEnvelopeRequest {
            bytes: encoded.envelope.clone(),
            limits: Limits::default(),
        },
        |response| match response {
            Response::ParseEnvelope(parsed) => *parsed,
            other => panic!("unexpected response {other:?}"),
        },
    )
    .expect("envelope parses");
    assert_eq!(parsed.nonce, nonce);
    assert_eq!(parsed.manifest, manifest);
    assert_eq!(parsed.manifest_bytes, built.manifest_bytes);
    assert_eq!(parsed.assertion, encode.assertion);
    assert!(!parsed.serial_present);
    assert_eq!(parsed.blobs.len(), 1);
    assert_eq!(parsed.blobs[0].kind, BlobKind::Samples);
    assert_eq!(parsed.blobs[0].sha256, sha256(&encode.blobs[0]));
    assert_eq!(parsed.pow, [0; POW_K]);
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

#[test]
fn solved_proof_of_work_verifies() {
    let challenge = Challenge::from_bytes([9; 32]);
    let params = PowParamsSpec {
        memory_kib: 8,
        iterations: 1,
        difficulty_bits_per_image: 0.0,
    };
    let response = call(
        Function::SolvePow,
        &SolvePowRequest {
            challenge,
            n_images: 1,
            params,
            start: 0,
        },
        |response| match response {
            Response::SolvePow(response) => response,
            other => panic!("unexpected response {other:?}"),
        },
    )
    .expect("proof-of-work solves");
    assert_eq!(response.solutions.len(), POW_K);
    verify(
        &challenge,
        1,
        &looklab_upload::PowParams {
            memory_kib: params.memory_kib,
            iterations: params.iterations,
            difficulty_bits_per_image: params.difficulty_bits_per_image,
        },
        &looklab_upload::PowSolutions::new(response.solutions),
    )
    .expect("solutions verify");
}

#[test]
fn a_truncated_envelope_is_a_structured_error() {
    let result = call(
        Function::ParseEnvelope,
        &ParseEnvelopeRequest {
            bytes: b"LLUPLD".to_vec(),
            limits: Limits::default(),
        },
        std::convert::identity,
    );
    assert!(matches!(result, Err(ModuleError::ParseEnvelope { .. })));
}

#[test]
fn a_request_with_trailing_bytes_is_rejected() {
    let input = postcard::to_allocvec(&VersionRequest)
        .expect("version request encodes")
        .into_iter()
        .chain([0])
        .collect::<Vec<u8>>();
    let output = dispatch(Function::Version.id(), &input);
    let result: Result<Response, ModuleError> =
        postcard::from_bytes(&output).expect("result decodes");
    assert!(matches!(
        result,
        Err(ModuleError::Request {
            function: Function::Version,
            ..
        })
    ));
}
