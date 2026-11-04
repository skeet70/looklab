// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;

use assert_cmd::Command;
use looklab_match::synth;
use looklab_model::LookDocument;
use looklab_model::realization::encode_crm;

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn ok(mut command: Command) -> String {
    let output = command.output().expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    stdout
}

fn write_document(path: &Path, slug: &str, look: &looklab_model::Look) {
    let identity = looklab_model::file::identity(slug, "1.0.0").expect("identity");
    let document = LookDocument::from_look(identity, look.clone()).expect("document");
    std::fs::write(path, document.encode().expect("encode")).expect("write look");
}

#[test]
fn match_prints_a_card_and_verify_runs_one_iteration() {
    let temp = tempfile::tempdir().expect("tempdir");
    let body = synth::body(9).expect("synthetic body");
    let probes = synth::probes();
    let target_path = temp.path().join("target.looklab");
    let base_path = temp.path().join("base.looklab");
    write_document(&target_path, "target", &body.model.base_look);
    write_document(&base_path, "base", &body.model.base_look);
    let crm_path = temp.path().join("crm.json");
    std::fs::write(&crm_path, encode_crm(&body.model.crm).expect("encode crm")).expect("write crm");
    let transform = looklab_cli::TransformFile {
        format: "looklab-transform/1".to_string(),
        version: 1,
        body: None,
        cct: None,
        camera_to_xyz: Some(synth::transform()),
        color_matrices: Vec::new(),
        forward_matrices: Vec::new(),
        per_image: Vec::new(),
    };
    let transform_path = temp.path().join("transform.yaml");
    std::fs::write(&transform_path, transform.to_yaml().expect("yaml")).expect("write transform");
    let permalink_path = temp.path().join("match.permalink");

    let stdout = ok({
        let mut command = looklab();
        command
            .args(["match", "--look"])
            .arg(&target_path)
            .arg("--body")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"))
            .arg("--crm")
            .arg(&crm_path)
            .arg("--base-look")
            .arg(&base_path)
            .arg("--transform")
            .arg(&transform_path)
            .arg("--permalink")
            .arg(&permalink_path)
            .args(["--emphasis", "skin"]);
        command
    });
    assert!(stdout.contains("base sim"), "{stdout}");
    assert!(stdout.contains("predicted ΔE"), "{stdout}");
    assert!(stdout.contains("confidence"), "{stdout}");
    assert!(permalink_path.exists());

    let observed_dir = temp.path().join("observed");
    std::fs::create_dir_all(&observed_dir).expect("observed dir");
    let samples = synth::observed(&body.model, &body.settings, &probes, 200).expect("observed");
    let bytes = looklab_samples::encode(&samples).expect("encode samples");
    std::fs::write(observed_dir.join("observed.llsamp"), bytes).expect("write samples");

    let stdout = ok({
        let mut command = looklab();
        command
            .arg("verify")
            .arg("--permalink")
            .arg(&permalink_path)
            .arg("--observed")
            .arg(&observed_dir)
            .arg("--look")
            .arg(&target_path)
            .arg("--body")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"))
            .arg("--crm")
            .arg(&crm_path)
            .arg("--base-look")
            .arg(&base_path)
            .arg("--transform")
            .arg(&transform_path);
        command
    });
    assert!(stdout.contains("verify:"), "{stdout}");
    assert!(stdout.contains("second suggestion"), "{stdout}");
}
