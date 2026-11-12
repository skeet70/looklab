// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;

use assert_cmd::Command;
use looklab_match::synth;
use looklab_model::LookDocument;
use looklab_model::realization::encode_crm;

#[derive(serde::Deserialize)]
struct VerificationResult {
    settings: looklab_model::realization::Settings,
}

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn ok(mut command: Command) {
    let output = command.output().expect("run");
    assert!(
        output.status.success(),
        "command failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn write_document(path: &Path, slug: &str, look: &looklab_model::Look) {
    let identity = looklab_model::file::identity(slug, "1.0.0").expect("identity");
    let document = LookDocument::from_look(identity, look.clone()).expect("document");
    std::fs::write(path, document.encode().expect("encode")).expect("write look");
}

fn flat_match(target: &Path, crm: &Path, base: &Path, transform: &Path) -> Command {
    let mut command = looklab();
    command
        .args(["match", "--look"])
        .arg(target)
        .arg("--body")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"))
        .arg("--crm")
        .arg(crm)
        .arg("--base-look")
        .arg(base)
        .arg("--transform")
        .arg(transform);
    command
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
    let output = looklab()
        .args(["match", "--look"])
        .arg(&target_path)
        .arg("--body")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"))
        .arg("--crm")
        .arg(&crm_path)
        .args(["--realization-version", "1"])
        .output()
        .expect("run");
    assert!(!output.status.success());

    let permalink_path = temp.path().join("match.permalink");

    let output = flat_match(&target_path, &crm_path, &base_path, &transform_path)
        .args(["--emphasis", "skin", "--seed", "7"])
        .output()
        .expect("run");
    let failed = !output.status.success();
    assert!(failed, "match accepted a missing realization version");
    let output = flat_match(&target_path, &crm_path, &base_path, &transform_path)
        .args([
            "--emphasis",
            "skin",
            "--seed",
            "7",
            "--realization-version",
            "0",
        ])
        .output()
        .expect("run");
    let failed = !output.status.success();
    assert!(failed, "match accepted a zero realization version");

    let mut command = flat_match(&target_path, &crm_path, &base_path, &transform_path);
    command.arg("--permalink").arg(&permalink_path).args([
        "--emphasis",
        "skin",
        "--seed",
        "7",
        "--realization-version",
        "1",
    ]);
    ok(command);
    let permalink_text = std::fs::read_to_string(&permalink_path).expect("permalink");
    let permalink = looklab_match::Permalink::parse(&permalink_text).expect("permalink");
    assert_eq!(permalink.seed, 7);
    assert_eq!(
        permalink.metric,
        looklab_match::ScoringMetric::RenderedPostClipV1
    );
    assert_eq!(permalink.optimizer, "looklab-match/2");
    let observed_dir = temp.path().join("observed");
    std::fs::create_dir_all(&observed_dir).expect("observed dir");
    let samples = synth::observed(&body.model, &body.settings, &probes, 200).expect("observed");
    let bytes = looklab_samples::encode(&samples).expect("encode samples");
    std::fs::write(observed_dir.join("observed.llsamp"), bytes).expect("write samples");
    let bad_permalink = temp.path().join("bad.permalink");
    std::fs::write(
        &bad_permalink,
        permalink_text.replace("optimizer=looklab-match/2", "optimizer=other/1"),
    )
    .expect("bad permalink");
    let output = looklab()
        .arg("verify")
        .arg("--permalink")
        .arg(&bad_permalink)
        .arg("--observed")
        .arg(&observed_dir)
        .output()
        .expect("run");
    assert!(!output.status.success());
    let bad_epoch = temp.path().join("bad-epoch.permalink");
    std::fs::write(
        &bad_epoch,
        permalink_text.replace("epoch_map=0", "epoch_map=1"),
    )
    .expect("bad epoch");
    let output = looklab()
        .arg("verify")
        .arg("--permalink")
        .arg(&bad_epoch)
        .arg("--observed")
        .arg(&observed_dir)
        .output()
        .expect("run");
    assert!(!output.status.success());

    ok({
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
    let updated_permalink = temp.path().join("match.updated.permalink");
    let updated_result = temp.path().join("match.updated.result.json");

    let result: VerificationResult =
        serde_json::from_slice(&std::fs::read(&updated_result).expect("result"))
            .expect("result json");
    let settings = result.settings;
    let observed_dir = temp.path().join("observed2");
    std::fs::create_dir_all(&observed_dir).expect("observed2 dir");
    let samples = synth::observed(&body.model, &settings, &probes, 200).expect("observed2");
    let bytes = looklab_samples::encode(&samples).expect("encode samples");
    std::fs::write(observed_dir.join("observed.llsamp"), bytes).expect("write samples");

    ok({
        let mut command = looklab();
        command
            .arg("verify")
            .arg("--permalink")
            .arg(&updated_permalink)
            .arg("--observed")
            .arg(&observed_dir);
        command
    });
}

#[test]
fn a_lut_target_prints_its_declared_base_and_destination_scope() {
    use looklab_model::file::{
        BaseReference, BaseStatus, DeclaredEncoding, LicenseRecord, LookVersion, LutData,
        LutDefinition, OutputEncoding, Slug,
    };
    use looklab_model::{LookKind, WbPolicy};

    let temp = tempfile::tempdir().expect("tempdir");
    let body = synth::body(9).expect("synthetic body");
    let base = BaseReference::Look {
        slug: Slug::new("base").expect("slug"),
        version: LookVersion::new("1.0.0").expect("version"),
    };
    let declared = BaseReference::Look {
        slug: Slug::new("declared-base").expect("slug"),
        version: LookVersion::new("1.0.0").expect("version"),
    };
    let mut values = Vec::new();
    for b in 0..2 {
        for g in 0..2 {
            for r in 0..2 {
                values.extend([f64::from(r), f64::from(g), f64::from(b)]);
            }
        }
    }
    let lut = LutData::new(2, [0.0; 3], [1.0; 3], values).expect("lut");
    let definition = LutDefinition {
        name: "Declared LUT".to_owned(),
        license: LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "a donor".to_owned(),
        },
        encoding: DeclaredEncoding::DisplayReferred {
            base: base.clone(),
            status: BaseStatus::Pinned,
        },
        declared_base: Some(declared),
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let identity = looklab_model::file::identity("lut-target", "1.0.0").expect("identity");
    let document = LookDocument::from_lut_definition(
        looklab_model::file::LookIdentity::new(
            identity.slug.as_str(),
            identity.version.as_str(),
            LookKind::LutDefined,
            WbPolicy::Adaptive,
        )
        .expect("identity"),
        looklab_model::Look::colorimetric(),
        definition,
        lut,
    )
    .expect("lut document");
    let target_path = temp.path().join("target.looklab");
    std::fs::write(&target_path, document.encode().expect("encode")).expect("write target");
    let base_path = temp.path().join("base.looklab");
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

    let output = flat_match(&target_path, &crm_path, &base_path, &transform_path)
        .args(["--realization-version", "1"])
        .output()
        .expect("run");
    assert!(
        output.status.success(),
        "match failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(
        text.contains(
            "the target's fitted coefficients approximate the declared base declared-base@1.0.0"
        ),
        "{text}"
    );
    assert!(
        text.contains("destination realization: the camera realization is unassessed"),
        "{text}"
    );
}
