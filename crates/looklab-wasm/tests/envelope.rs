// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_samples::{
    InputSpace, OutputSpace, PrepParamsHash, PrepVersion, PriorTransformVersion, SampleFlags,
    SampleHeader, SampleSet,
};
use looklab_upload::{
    BatchManifest, BlobKind, BodyId, CommunitySnapshotVersion, ContributionKind, Intent, Limits,
    ManifestMetadata, MonochromeAuthorization, POW_K, RenderClass, SimKey,
};
use looklab_wasm::prep::ModuleFunction;
use looklab_wasm::{
    AlignmentSummary, AssertionFields, ControlSetting, GroupSettings, IntentChoice,
    PreparedImageView, build_manifest_request, build_send_manifest_request,
    decode_build_manifest_reply, decode_build_send_manifest_reply, decode_encode_reply,
    decode_parse_reply, encode_envelope_request, module_fn_id, parse_envelope_request,
};

fn call(function: ModuleFunction, request: &[u8]) -> Vec<u8> {
    looklab_prep_module::dispatch(module_fn_id(function), request)
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
        prep_version: PrepVersion::new(looklab_prep::PREP_VERSION),
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

fn image(blob: Vec<u8>) -> PreparedImageView {
    PreparedImageView {
        blob,
        metadata: metadata(),
        removed: Vec::new(),
        rerender_group: None,
        alignment: AlignmentSummary {
            correlation: 1.0,
            residual_px: 0.0,
        },
    }
}

fn settings(render_class: RenderClass) -> GroupSettings {
    GroupSettings {
        kind: ContributionKind::Library,
        intent: IntentChoice::Native,
        render_class,
        monochrome_authorization: match render_class {
            RenderClass::Colour => None,
            RenderClass::Monochrome => Some(MonochromeAuthorization::ModelDevelopment180Days),
        },
        body: "fujifilm/x-t5".to_owned(),
        firmware: "2.10".to_owned(),
        sim: match render_class {
            RenderClass::Colour => "provia",
            RenderClass::Monochrome => "acros",
        }
        .to_owned(),
        community_snapshot_version: CommunitySnapshotVersion::SHIPPED.get(),
        controls: vec![ControlSetting {
            key: "highlight_tone".to_owned(),
            step: 0,
            source: looklab_upload::SettingSource::Declared,
        }],
    }
}

fn built(render_class: RenderClass) -> (GroupSettings, Vec<u8>, Vec<u8>) {
    let settings = settings(render_class);
    let images = vec![image(sample_blob())];
    let nonce = [3u8; 32];
    let request = build_manifest_request(&settings, &images, nonce).expect("request builds");
    let reply = call(ModuleFunction::BuildManifest, &request);
    (settings, request, reply)
}

#[test]
fn the_signed_fields_reach_the_manifest() {
    let (settings, request, reply) = built(RenderClass::Monochrome);
    let response = decode_build_manifest_reply(&reply).expect("manifest builds");
    let manifest: BatchManifest =
        postcard::from_bytes(&response.manifest_bytes).expect("manifest bytes decode");
    assert_eq!(manifest.render_class, RenderClass::Monochrome);
    assert_eq!(
        manifest.monochrome_authorization,
        Some(MonochromeAuthorization::ModelDevelopment180Days)
    );
    assert_eq!(manifest.kind, settings.kind);
    assert_eq!(manifest.sim_key, SimKey::new("acros").expect("sim key"));
    assert_eq!(manifest.settings.entries().len(), 1);

    let colour = decode_build_manifest_reply(&call(ModuleFunction::BuildManifest, &request))
        .expect("manifest builds");
    assert_eq!(
        colour.manifest_bytes, response.manifest_bytes,
        "the same request builds the same manifest"
    );
}

#[test]
fn envelope_construction_is_deterministic_and_round_trips() {
    let nonce = [3u8; 32];
    let (_, _, manifest_reply) = built(RenderClass::Monochrome);
    let manifest = decode_build_manifest_reply(&manifest_reply).expect("manifest builds");

    let send_request = build_send_manifest_request(nonce, vec![manifest.manifest_hash], Vec::new())
        .expect("request builds");
    let send_reply = call(ModuleFunction::BuildSendManifest, &send_request);
    let send = decode_build_send_manifest_reply(&send_reply).expect("send manifest builds");
    assert_eq!(
        send_reply,
        call(ModuleFunction::BuildSendManifest, &send_request),
        "the send manifest is deterministic"
    );

    let blob = sample_blob();
    let assertion = AssertionFields {
        credential_id: vec![1],
        authenticator_data: vec![0; 37],
        client_data_json: b"{}".to_vec(),
        signature: vec![2; 64],
    };
    let request = encode_envelope_request(
        nonce,
        send.send_manifest_bytes.clone(),
        manifest.manifest_bytes.clone(),
        assertion.clone(),
        [0; POW_K],
        None,
        vec![blob],
    )
    .expect("request builds");
    let first = decode_encode_reply(&call(ModuleFunction::EncodeEnvelope, &request))
        .expect("envelope encodes");
    let second = decode_encode_reply(&call(ModuleFunction::EncodeEnvelope, &request))
        .expect("envelope encodes");
    assert_eq!(first.envelope, second.envelope);
    assert_eq!(first.manifest_hash, manifest.manifest_hash);

    let parsed = decode_parse_reply(&call(
        ModuleFunction::ParseEnvelope,
        &parse_envelope_request(first.envelope.clone(), Limits::default()),
    ))
    .expect("envelope parses");
    assert_eq!(parsed.manifest.render_class, RenderClass::Monochrome);
    assert_eq!(
        parsed.manifest.monochrome_authorization,
        Some(MonochromeAuthorization::ModelDevelopment180Days)
    );
    assert_eq!(parsed.manifest_bytes, manifest.manifest_bytes);
    assert_eq!(parsed.blobs.len(), 1);
    assert_eq!(parsed.blobs[0].kind, BlobKind::Samples);
    assert_eq!(parsed.assertion, assertion);
}

#[test]
fn a_colour_group_sends_no_authorization() {
    let (_, _, reply) = built(RenderClass::Colour);
    let response = decode_build_manifest_reply(&reply).expect("manifest builds");
    let manifest: BatchManifest =
        postcard::from_bytes(&response.manifest_bytes).expect("manifest bytes decode");
    assert_eq!(manifest.render_class, RenderClass::Colour);
    assert_eq!(manifest.monochrome_authorization, None);
    assert_eq!(
        manifest.body,
        BodyId::new("fujifilm/x-t5").expect("body id")
    );
    assert_eq!(manifest.intent, Intent::Native);
}
