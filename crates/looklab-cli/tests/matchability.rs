// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use looklab_color::Mat3;
use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, DeclaredEncoding, LicenseRecord, LutData, LutDefinition,
    OutputEncoding,
};
use looklab_model::{
    Look, LookDocument, LookIdentity, LookKind, ProbeScalars, UncertaintyData, WbPolicy,
};

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn identity(slug: &str, kind: LookKind) -> LookIdentity {
    LookIdentity::new(slug, "1.0.0", kind, WbPolicy::Adaptive).expect("identity")
}

fn native_without_support() -> LookDocument {
    let probes = looklab_model::probes::v1();
    LookDocument::from_look(
        identity("native-unsupported", LookKind::NativeFamily),
        Look::colorimetric(),
    )
    .expect("document")
    .with_uncertainty(UncertaintyData {
        support: Some(ProbeScalars::new(vec![0.0; probes.probes.len()]).expect("support")),
        ..UncertaintyData::default()
    })
    .expect("uncertainty")
}

fn display_unpinned() -> LookDocument {
    let lut = LutData::new(2, [0.0; 3], [1.0; 3], vec![0.0; 24]).expect("lut");
    let definition = LutDefinition {
        name: "Pending base".to_string(),
        license: LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "test".to_string(),
        },
        encoding: DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status: BaseStatus::BaseIncomplete {
                missing: BaseField::FilmSimulation,
            },
        },
        declared_base: None,
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    LookDocument::from_lut_definition(
        identity("display-unpinned", LookKind::LutDefined),
        Look::colorimetric(),
        definition,
        lut,
    )
    .expect("document")
}

fn write_document(path: &Path, document: &LookDocument) {
    std::fs::write(path, document.encode().expect("encode")).expect("write look");
}

fn write_transform(path: &Path) {
    let transform = looklab_cli::TransformFile {
        format: "looklab-transform/1".to_string(),
        version: 1,
        body: None,
        cct: None,
        camera_to_xyz: Some(Mat3::IDENTITY),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image: Vec::new(),
    };
    std::fs::write(path, transform.to_yaml().expect("yaml")).expect("write transform");
}

fn export(look: &Path, out: &Path, force: bool) -> Output {
    let transform = look.parent().expect("parent").join("transform.yaml");
    let profile = workspace().join("data/interfaces/example-app-lut3d-v1.yaml");
    let mut command = looklab();
    command
        .args(["export"])
        .arg(look)
        .arg("--profile")
        .arg(profile)
        .arg("--body-transform")
        .arg(transform)
        .args(["--format", "cube", "--out"])
        .arg(out);
    if force {
        command.arg("--force");
    }
    command.output().expect("run export")
}

fn assert_refusal(output: &Output) {
    assert!(
        !output.status.success(),
        "the command succeeded: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn export_refuses_a_native_look_without_supported_probes() {
    let temp = tempfile::tempdir().expect("tempdir");
    let look = temp.path().join("native.looklab");
    write_document(&look, &native_without_support());
    write_transform(&temp.path().join("transform.yaml"));
    let out = temp.path().join("native.cube");

    assert_refusal(&export(&look, &out, false));
    assert!(!out.exists(), "a rejected export wrote {}", out.display());

    assert_refusal(&export(&look, &out, true));
    assert!(!out.exists(), "force bypassed the rejection");
}

#[test]
fn export_refuses_an_unpinned_display_base() {
    let temp = tempfile::tempdir().expect("tempdir");
    let look = temp.path().join("display.looklab");
    write_document(&look, &display_unpinned());
    write_transform(&temp.path().join("transform.yaml"));
    let out = temp.path().join("display.cube");

    assert_refusal(&export(&look, &out, false));
    assert!(!out.exists(), "a rejected export wrote {}", out.display());

    assert_refusal(&export(&look, &out, true));
    assert!(!out.exists(), "force bypassed the rejection");
}
