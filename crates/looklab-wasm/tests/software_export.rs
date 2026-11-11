// SPDX-License-Identifier: AGPL-3.0-or-later
//! Public software-export API: gating, formats and deterministic bytes.

use looklab_export::{ExportError, read_cube, read_cube_1d, read_haldclut, read_native_json};
use looklab_model::file::identity;
use looklab_model::{
    Look, LookDocument, MatchRefusal, ProbeScalars, UncertaintyData, V1_PROBE_COUNT,
};
use looklab_publish::InputTransformVersion;
use looklab_wasm::{
    CctK, ExportFormat, SoftwareExportError, SoftwareExportRequest, SoftwareExportVersion,
    encode_software_export_request, run_export,
};

#[test]
fn export_engine_identity_is_a_stable_string_on_the_worker_wire() {
    let wire = postcard::to_allocvec(&SoftwareExportVersion::CURRENT).expect("encode version");
    let identity: String = postcard::from_bytes(&wire).expect("decode engine identity");
    assert_eq!(identity, "looklab-software-export/1");
    let unsupported =
        postcard::to_allocvec(&"looklab-software-export/99").expect("encode unknown version");
    assert!(postcard::from_bytes::<SoftwareExportVersion>(&unsupported).is_err());
}

fn look_bytes() -> Vec<u8> {
    LookDocument::from_look(
        identity("export-target", "1.0.0").expect("identity"),
        Look::colorimetric(),
    )
    .expect("document")
    .encode()
    .expect("encode")
}

fn unsupported_look_bytes() -> Vec<u8> {
    let document = LookDocument::from_look(
        identity("export-target", "1.0.0").expect("identity"),
        Look::colorimetric(),
    )
    .expect("document");
    let uncertainty = UncertaintyData {
        support: Some(ProbeScalars::new(vec![0.0; V1_PROBE_COUNT]).expect("support mask")),
        ..UncertaintyData::default()
    };
    document
        .with_uncertainty(uncertainty)
        .expect("uncertainty")
        .encode()
        .expect("encode")
}

fn scene_profile(
    handling: &str,
    sizes: &str,
    transfer: &str,
    with_shaper: bool,
    formats: &str,
) -> String {
    format!(
        r#"id: test/scene-xyz
name: "Scene XYZ test pipeline"
version: 1
insertion:
  point: scene_linear_xyz_d65
  encoding:
    transfer: {transfer}
    shaper: {{ min_ev: -12, max_ev: 6, mid_gray: 0.18 }}
    primaries: xyz_d65
    range: [0.0, 1.0]
  illuminant_handling: {handling}
accepts:
  - type: lut3d
    sizes: [{sizes}]
    interpolation: tetrahedral
    precision: f32
    with_shaper: {with_shaper}
downstream:
  - output_transform: {{ primaries: srgb, transfer: srgb }}
  - gamut_clip: hard
formats: {formats}
limits: {{ max_bytes: 67108864 }}
"#
    )
}

fn camera_profile() -> String {
    r#"id: test/camera
name: "Camera test pipeline"
version: 1
bodies: [vendor/model-a]
insertion:
  point: camera_rgb_wb_linear
  encoding:
    transfer: linear
    primaries: camera_native
    range: [0.0, 8.0]
  illuminant_handling: single
accepts:
  - type: lut3d
    sizes: [33]
    interpolation: tetrahedral
    precision: f32
    with_shaper: false
downstream:
  - output_transform: { primaries: srgb, transfer: srgb }
  - gamut_clip: hard
formats: [cube]
limits: { max_bytes: 67108864 }
"#
    .to_owned()
}

fn camera_lut_profile() -> String {
    r#"id: test/camera-lut
name: "Camera LUT test pipeline"
version: 1
bodies: [vendor/model-a]
insertion:
  point: camera_rgb_wb_linear
  encoding:
    transfer: linear
    primaries: camera_native
    range: [0.0, 8.0]
  illuminant_handling: single
accepts:
  - type: lut3d
    sizes: [65]
    interpolation: tetrahedral
    precision: f32
    with_shaper: true
downstream:
  - output_transform: { primaries: srgb, transfer: srgb }
  - gamut_clip: hard
formats: [cube]
limits: { max_bytes: 67108864 }
"#
    .to_owned()
}

fn camera_transform() -> String {
    r#"format: looklab-transform/1
version: 1
body: vendor/model-a
color_matrices:
  - cct: 2850.0
    xyz_to_camera:
      - [1.6064, -0.9942, 0.112]
      - [-0.376, 1.2096, 0.1853]
      - [-0.0007, 0.0545, 0.6654]
  - cct: 6500.0
    xyz_to_camera:
      - [1.3426, -0.6334, -0.1177]
      - [-0.4244, 1.2136, 0.2371]
      - [-0.058, 0.1303, 0.598]
"#
    .to_owned()
}

fn no_supported_profile() -> String {
    r#"id: test/no-supported
name: "No supported probe test pipeline"
version: 1
insertion:
  point: scene_linear_rec2020
  encoding:
    transfer: linear
    primaries: rec2020
    range: [0.0, 0.000000000001]
  illuminant_handling: single
accepts:
  - type: lut3d
    sizes: [17]
    interpolation: tetrahedral
    precision: f32
    with_shaper: false
downstream:
  - output_transform: { primaries: srgb, transfer: srgb }
  - gamut_clip: hard
formats: [cube]
limits: { max_bytes: 67108864 }
"#
    .to_owned()
}

fn no_artifact_profile() -> String {
    r#"id: test/no-artifact
name: "No artifact test pipeline"
version: 1
insertion:
  point: scene_linear_xyz_d65
  encoding:
    transfer: linear
    primaries: xyz_d65
    range: [0.0, 1.0]
  illuminant_handling: single
accepts: []
downstream:
  - output_transform: { primaries: srgb, transfer: srgb }
  - gamut_clip: hard
formats: [cube]
limits: { max_bytes: 67108864 }
"#
    .to_owned()
}

fn request(profile_yaml: String, format: ExportFormat) -> SoftwareExportRequest {
    SoftwareExportRequest {
        version: SoftwareExportVersion::V1,
        target_look: look_bytes(),
        profile_yaml,
        transform_yaml: None,
        body: None,
        format,
        cct: CctK::D65,
    }
}

fn body_id(value: &str) -> looklab_schema::BodyId {
    looklab_schema::BodyId::new(value).expect("body id")
}

fn with_declared_body(profile: String) -> String {
    profile.replace("version: 1\n", "version: 1\nbodies: [vendor/model-a]\n")
}

#[test]
fn camera_insertion_without_calibration_is_rejected() {
    let mut request = request(camera_profile(), ExportFormat::Cube);
    request.body = Some(body_id("vendor/model-a"));
    let error = run_export(&request).expect_err("no transform");
    assert!(matches!(error, SoftwareExportError::MissingCameraTransform));
}

#[test]
fn a_camera_transform_must_name_a_declared_body() {
    let transform = r#"format: looklab-transform/1
version: 1
body: other/model
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
"#;
    let mut request = request(camera_profile(), ExportFormat::Cube);
    request.body = Some(body_id("vendor/model-a"));
    request.transform_yaml = Some(transform.to_owned());
    let error = run_export(&request).expect_err("undeclared body");
    assert!(matches!(
        error,
        SoftwareExportError::TransformBodyMismatch { .. }
    ));
}

#[test]
fn a_dng_camera_calibration_bakes_a_simulated_export() {
    let mut request = request(camera_lut_profile(), ExportFormat::Cube);
    request.body = Some(body_id("vendor/model-a"));
    request.transform_yaml = Some(camera_transform());
    let reply = run_export(&request).expect("camera export");
    assert!(!reply.flagged, "{:?}", reply.validation.primary);
    assert!(reply.validation.primary.stats.p95 <= 1.0);
    assert_eq!(
        reply.input_transform_version,
        Some(InputTransformVersion::new(1).expect("version"))
    );
    assert_eq!(reply.files.len(), 2);
}

#[test]
fn camera_insertion_requires_a_selected_body() {
    let transform = r#"format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
"#;
    let profile = camera_profile().replace("bodies: [vendor/model-a]\n", "");
    let mut request = request(profile, ExportFormat::Cube);
    request.transform_yaml = Some(transform.to_owned());
    assert!(matches!(
        run_export(&request),
        Err(SoftwareExportError::MissingSelectedBody)
    ));
}

#[test]
fn a_declared_body_is_required_without_a_transform() {
    let profile = with_declared_body(scene_profile(
        "single",
        "33",
        "log2_shaper",
        false,
        "[cube]",
    ));
    let mut request = request(profile, ExportFormat::Cube);
    assert!(matches!(
        run_export(&request),
        Err(SoftwareExportError::MissingSelectedBody)
    ));
    request.body = Some(body_id("vendor/model-a"));
    run_export(&request).expect("fixed scene export");
}

#[test]
fn an_encoded_request_preserves_optional_field_alignment() {
    let profile = with_declared_body(scene_profile("single", "65", "log2_shaper", true, "[cube]"));
    let mut request = request(profile, ExportFormat::Cube);
    request.body = Some(body_id("vendor/model-a"));
    let wire = encode_software_export_request(&request);
    let decoded: SoftwareExportRequest =
        postcard::from_bytes(&wire).expect("worker request decodes");
    let reply = run_export(&decoded).expect("encoded fixed scene export");
    assert!(!reply.flagged, "{:?}", reply.validation.primary);
    assert_eq!(reply.input_transform_version, None);

    let mut transform_only = request;
    transform_only.body = None;
    transform_only.profile_yaml = scene_profile("single", "65", "log2_shaper", true, "[cube]");
    transform_only.transform_yaml = Some(camera_transform());
    let wire = encode_software_export_request(&transform_only);
    let decoded: SoftwareExportRequest =
        postcard::from_bytes(&wire).expect("worker request decodes");
    assert_eq!(decoded, transform_only);
    assert!(matches!(
        run_export(&decoded),
        Err(SoftwareExportError::UnusedTransform)
    ));
}

#[test]
fn a_transform_for_a_scene_insertion_is_rejected() {
    let transform = r#"format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
"#;
    let mut request = request(
        scene_profile("single", "33", "log2_shaper", false, "[cube]"),
        ExportFormat::Cube,
    );
    request.transform_yaml = Some(transform.to_owned());
    assert!(matches!(
        run_export(&request),
        Err(SoftwareExportError::UnusedTransform)
    ));
}

#[test]
fn body_agnostic_dual_export_writes_both_illuminants_and_shapers() {
    let reply = run_export(&request(
        scene_profile("dual_interpolated", "65", "linear", true, "[cube]"),
        ExportFormat::Cube,
    ))
    .expect("export");
    assert!(!reply.flagged, "{:?}", reply.validation.primary);
    assert_eq!(reply.input_transform_version, None);
    assert!(reply.validation.primary.stats.p95 <= 1.0);
    assert_eq!(reply.files.len(), 4);
    for cct in [2856_u32, 6500] {
        assert!(
            reply
                .files
                .iter()
                .any(|file| file.name.as_str().contains(&format!("-{cct}.cube")))
        );
        assert!(
            reply
                .files
                .iter()
                .any(|file| file.name.as_str().contains(&format!("-{cct}-shaper.cube")))
        );
    }
    for file in &reply.files {
        let text = std::str::from_utf8(&file.bytes).expect("utf8");
        if file.name.as_str().ends_with("-shaper.cube") {
            let shaper = read_cube_1d(text).expect("shaper");
            assert_eq!(shaper.domain, [0.0, 1.0]);
        } else {
            let grid = read_cube(text).expect("grid");
            assert_eq!(grid.domain, [0.0, 1.0]);
            assert_eq!(grid.lut.size, 65);
        }
    }
}

#[test]
fn native_json_and_haldclut_are_written() {
    for format in [ExportFormat::Haldclut, ExportFormat::NativeJson] {
        let formats = format!("[{format}]");
        let reply = run_export(&request(
            scene_profile("single", "65", "log2_shaper", false, &formats),
            format,
        ))
        .expect("export");
        assert!(!reply.flagged, "{format}: {:?}", reply.validation.primary);
        assert_eq!(reply.files.len(), 1);
        assert!(!reply.files[0].bytes.is_empty());
        if format == ExportFormat::Haldclut {
            let lut = read_haldclut(&reply.files[0].bytes).expect("haldclut");
            assert!(lut.size >= 65);
        } else {
            let text = std::str::from_utf8(&reply.files[0].bytes).expect("utf8");
            let output = read_native_json(text).expect("native json");
            assert_eq!(output.artifacts.len(), 1);
        }
    }
}

#[test]
fn declared_format_and_artifact_support_are_enforced() {
    let mut request = request(
        scene_profile("single", "33", "log2_shaper", false, "[native_json]"),
        ExportFormat::Cube,
    );
    assert!(matches!(
        run_export(&request),
        Err(SoftwareExportError::FormatNotDeclared(ExportFormat::Cube))
    ));
    request.profile_yaml = no_artifact_profile();
    assert!(matches!(
        run_export(&request),
        Err(SoftwareExportError::Export(ExportError::NoAcceptedArtifact))
    ));
}

#[test]
fn a_flagged_export_returns_validation_without_files() {
    let reply = run_export(&request(
        scene_profile("single", "2", "log2_shaper", false, "[cube]"),
        ExportFormat::Cube,
    ))
    .expect("reply");
    assert!(reply.flagged);
    assert!(reply.files.is_empty());
    assert!(reply.validation.primary.stats.p95 > 1.0);
}

#[test]
fn repeated_exports_are_byte_identical() {
    let request = request(
        scene_profile("single", "65", "log2_shaper", false, "[cube]"),
        ExportFormat::Cube,
    );
    let first = run_export(&request).expect("first");
    let second = run_export(&request).expect("second");
    assert!(!first.flagged, "{:?}", first.validation.primary);
    assert_eq!(first.version, request.version);
    assert_eq!(first, second);
    assert_eq!(first.files.len(), 1);
}

#[test]
fn emitted_files_never_exceed_the_declared_byte_limit() {
    let profile = scene_profile("single", "65", "linear", true, "[cube]");
    let reply = run_export(&request(profile.clone(), ExportFormat::Cube)).expect("export");
    assert!(!reply.flagged, "{:?}", reply.validation.primary);
    assert_eq!(reply.files.len(), 2);
    let total: u64 = reply.files.iter().map(|file| file.bytes.len() as u64).sum();
    let limit = total - 1;
    let tight = profile.replace(
        "limits: { max_bytes: 67108864 }",
        &format!("limits: {{ max_bytes: {limit} }}"),
    );
    let error = run_export(&request(tight, ExportFormat::Cube)).expect_err("over the ceiling");
    assert!(matches!(
        error,
        SoftwareExportError::Export(ExportError::ByteLimit { needed, limit: declared })
            if needed == total && declared == limit
    ));
}

#[test]
fn a_zero_supported_probe_export_is_rejected() {
    let error = run_export(&request(no_supported_profile(), ExportFormat::Cube))
        .expect_err("no supported probes");
    assert!(matches!(
        error,
        SoftwareExportError::NoSupportedProbes { .. }
    ));
}

#[test]
fn a_support_mask_that_admits_no_probe_is_rejected() {
    let mut request = request(
        scene_profile("single", "33", "log2_shaper", false, "[cube]"),
        ExportFormat::Cube,
    );
    request.target_look = unsupported_look_bytes();
    let error = run_export(&request).expect_err("unsupported mask");
    assert!(matches!(
        error,
        SoftwareExportError::NotMatchable(MatchRefusal::Unsupported)
    ));
}
