// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Instant;

use assert_cmd::Command;
use looklab_match::synth::{self, SyntheticBody};
use looklab_model::LookDocument;
use looklab_model::file::{LookVersion, Slug, identity};
use looklab_model::realization::encode_crm;
use looklab_publish::{
    ArtifactBlob, ArtifactName, GateSummary, LookStatus, MANIFEST_NAME, MatchBundle, MatchSim,
    ProvenanceSummary, VersionManifest, body_artifact, bundle_artifact, sim_artifacts,
    transform_artifact,
};
use looklab_schema::{BodyId, SimKey};

fn looklab() -> Command {
    Command::cargo_bin("looklab").expect("built binary")
}

fn encode_document(slug: &str, look: &looklab_model::Look) -> Vec<u8> {
    LookDocument::from_look(identity(slug, "1.0.0").expect("identity"), look.clone())
        .expect("document")
        .encode()
        .expect("encode")
}

struct Fixture {
    temp: tempfile::TempDir,
    dir: PathBuf,
    target: PathBuf,
    body: BodyId,
    classic: SyntheticBody,
    provia: SyntheticBody,
}

fn published_bundle() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let dir = temp.path().join("bundle");
    std::fs::create_dir_all(&dir).expect("bundle dir");

    let classic = synth::body(9).expect("synthetic body");
    let provia_key = SimKey::new("provia").expect("sim");
    let provia = synth::body_for_sim(10, &provia_key).expect("synthetic body");
    let body = BodyId::new("fujifilm/x-t5").expect("body");

    let target = temp.path().join("target.looklab");
    std::fs::write(&target, encode_document("target", &provia.model.base_look)).expect("target");

    let schema_name = body_artifact(&body, 1).expect("name");
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"),
        dir.join(schema_name.as_str()),
    )
    .expect("body schema");

    let transform_name = transform_artifact(&body, 1).expect("name");
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
    std::fs::write(
        dir.join(transform_name.as_str()),
        transform.to_yaml().expect("yaml"),
    )
    .expect("transform");

    let mut sims = Vec::new();
    for (model, slug) in [(&classic, "base-classic-chrome"), (&provia, "base-provia")] {
        let names = sim_artifacts(&body, 1, &model.model.crm.base_sim).expect("names");
        std::fs::write(
            dir.join(names.base.as_str()),
            encode_document(slug, &model.model.base_look),
        )
        .expect("base");
        std::fs::write(
            dir.join(names.crm.as_str()),
            encode_crm(&model.model.crm).expect("crm"),
        )
        .expect("crm");
        sims.push(MatchSim {
            sim: model.model.crm.base_sim.clone(),
            base: names.base,
            crm: names.crm,
            covariance: None,
        });
    }
    let bundle = MatchBundle::new(body.clone(), 1, sims, Vec::new()).expect("bundle");
    let descriptor_name = bundle_artifact(&body, 1).expect("name");
    std::fs::write(
        dir.join(descriptor_name.as_str()),
        bundle.to_json().expect("json"),
    )
    .expect("descriptor");

    let fixture = Fixture {
        temp,
        dir,
        target,
        body,
        classic,
        provia,
    };
    write_manifest(&fixture.dir, &published_files(&fixture));
    fixture
}

fn published_files(fixture: &Fixture) -> Vec<(ArtifactName, Vec<u8>)> {
    let mut names = vec![
        bundle_artifact(&fixture.body, 1).expect("name"),
        body_artifact(&fixture.body, 1).expect("name"),
        transform_artifact(&fixture.body, 1).expect("name"),
    ];
    for model in [&fixture.classic, &fixture.provia] {
        let sim_names = sim_artifacts(&fixture.body, 1, &model.model.crm.base_sim).expect("names");
        names.push(sim_names.base);
        names.push(sim_names.crm);
    }
    names
        .into_iter()
        .map(|name| {
            let bytes = std::fs::read(fixture.dir.join(name.as_str())).expect("artifact");
            (name, bytes)
        })
        .collect()
}

fn write_manifest(dir: &Path, artifacts: &[(ArtifactName, Vec<u8>)]) {
    let files: Vec<ArtifactBlob> = artifacts
        .iter()
        .map(|(name, bytes)| ArtifactBlob::new(name.clone(), bytes.clone()))
        .collect();
    let manifest = VersionManifest::from_files(
        Slug::new("target").expect("slug"),
        LookVersion::new("1.0.0").expect("version"),
        LookStatus::Published,
        GateSummary::default(),
        ProvenanceSummary::default(),
        &files,
    )
    .expect("manifest");
    std::fs::write(
        dir.join(MANIFEST_NAME),
        manifest.to_json().expect("manifest json"),
    )
    .expect("manifest");
}

fn run_match(fixture: &Fixture) -> Output {
    looklab()
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.dir)
        .args(["--seed", "7", "--realization-version", "1"])
        .output()
        .expect("run")
}

fn assert_rejected(output: Output) {
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        !output.status.success(),
        "command succeeded\nstdout: {stdout}\nstderr: {stderr}"
    );
}

#[test]
fn matching_a_legacy_bundle_requires_a_positive_realization_version() {
    let fixture = published_bundle();
    let run = |version: Option<&str>| {
        let mut command = looklab();
        command
            .arg("match")
            .arg("--look")
            .arg(&fixture.target)
            .arg("--bundle")
            .arg(&fixture.dir)
            .args(["--seed", "7"]);
        if let Some(version) = version {
            command.args(["--realization-version", version]);
        }
        command.output().expect("run")
    };
    let missing = run(None);
    let failed = !missing.status.success();
    let stderr = String::from_utf8(missing.stderr).expect("utf-8 stderr");
    assert!(failed, "match accepted a missing realization version");
    assert!(
        stderr.contains("--realization-version is required"),
        "{stderr}"
    );
    let zero = run(Some("0"));
    let failed = !zero.status.success();
    let stderr = String::from_utf8(zero.stderr).expect("utf-8 stderr");
    assert!(failed, "match accepted a zero realization version");
    assert!(stderr.contains("must be positive"), "{stderr}");
}

#[test]
fn a_published_bundle_ranks_every_sim_and_picks_the_matching_one() {
    let fixture = published_bundle();
    let permalink_path = fixture.temp.path().join("match.permalink");
    let started = Instant::now();
    let output = looklab()
        .current_dir(fixture.temp.path())
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.dir)
        .arg("--permalink")
        .arg("match.permalink")
        .args(["--seed", "7", "--realization-version", "1"])
        .output()
        .expect("run");
    let elapsed = started.elapsed();
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    println!("multi-sim match wall time: {elapsed:?}");
    let text = std::fs::read_to_string(&permalink_path).expect("permalink");
    let permalink = looklab_match::Permalink::parse(&text).expect("canonical permalink");
    assert_eq!(
        SimKey::new(&permalink.sim).expect("sim"),
        fixture.provia.model.crm.base_sim
    );
    assert_eq!(
        Slug::new(&permalink.base_slug).expect("base slug"),
        Slug::new("base-provia").expect("slug")
    );
    for source in [
        permalink.sources.target_look,
        permalink.sources.base_look,
        permalink.sources.body,
        permalink.sources.crm,
        permalink.sources.transform,
    ] {
        let source = source.expect("pinned source");
        assert!(Path::new(&source).is_relative(), "{source}");
    }
}

#[test]
fn a_base_look_that_differs_from_its_manifest_entry_is_rejected() {
    let fixture = published_bundle();
    let names = sim_artifacts(&fixture.body, 1, &fixture.provia.model.crm.base_sim).expect("names");
    std::fs::write(
        fixture.dir.join(names.base.as_str()),
        encode_document("other-base", &fixture.provia.model.base_look),
    )
    .expect("tampered base");
    assert_rejected(run_match(&fixture));
}

#[test]
fn a_crm_that_differs_from_its_manifest_entry_is_rejected() {
    let fixture = published_bundle();
    let names = sim_artifacts(&fixture.body, 1, &fixture.provia.model.crm.base_sim).expect("names");
    let mut crm = fixture.provia.model.crm.clone();
    crm.version += 1;
    std::fs::write(
        fixture.dir.join(names.crm.as_str()),
        encode_crm(&crm).expect("crm"),
    )
    .expect("tampered crm");
    assert_rejected(run_match(&fixture));
}

#[test]
fn a_crm_from_another_body_is_rejected_even_when_the_manifest_matches() {
    let fixture = published_bundle();
    let names = sim_artifacts(&fixture.body, 1, &fixture.provia.model.crm.base_sim).expect("names");
    let mut crm = fixture.provia.model.crm.clone();
    crm.body = BodyId::new("nikon/z8").expect("body");
    std::fs::write(
        fixture.dir.join(names.crm.as_str()),
        encode_crm(&crm).expect("crm"),
    )
    .expect("wrong-body crm");
    write_manifest(&fixture.dir, &published_files(&fixture));
    assert_rejected(run_match(&fixture));
}

#[test]
fn a_bundle_without_a_version_manifest_is_rejected() {
    let fixture = published_bundle();
    std::fs::remove_file(fixture.dir.join(MANIFEST_NAME)).expect("remove manifest");
    assert_rejected(run_match(&fixture));
}

#[test]
fn a_descriptor_cannot_name_artifacts_outside_its_version_directory() {
    let fixture = published_bundle();
    let descriptor = fixture
        .dir
        .join(bundle_artifact(&fixture.body, 1).expect("name").as_str());
    let mut bundle =
        MatchBundle::decode(&std::fs::read(&descriptor).expect("descriptor")).expect("bundle");
    bundle.sims[0].base = ArtifactName::new("..").expect("name");
    std::fs::write(&descriptor, bundle.to_json().expect("json")).expect("descriptor");
    write_manifest(&fixture.dir, &published_files(&fixture));
    assert_rejected(run_match(&fixture));
}

fn write_observed(fixture: &Fixture) -> PathBuf {
    let dir = fixture.temp.path().join("observed");
    std::fs::create_dir_all(&dir).expect("observed dir");
    let samples = synth::observed(
        &fixture.provia.model,
        &fixture.provia.settings,
        &synth::probes(),
        200,
    )
    .expect("observed");
    std::fs::write(
        dir.join("observed.llsamp"),
        looklab_samples::encode(&samples).expect("encode samples"),
    )
    .expect("write samples");
    dir
}

fn run_verify(permalink: &Path, observed: &Path) -> Output {
    looklab()
        .arg("verify")
        .arg("--permalink")
        .arg(permalink)
        .arg("--observed")
        .arg(observed)
        .output()
        .expect("run")
}

#[test]
fn verify_replays_the_pinned_bundle_beside_its_permalink() {
    let fixture = published_bundle();
    let permalink = fixture.temp.path().join("match.permalink");
    let output = looklab()
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.dir)
        .arg("--permalink")
        .arg(&permalink)
        .args(["--seed", "7", "--realization-version", "1"])
        .output()
        .expect("run");
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "command failed\nstdout: {stdout}\nstderr: {stderr}"
    );

    let observed = write_observed(&fixture);
    let output = run_verify(&permalink, &observed);
    let stdout = String::from_utf8(output.stdout).expect("utf-8 stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        output.status.success(),
        "verify failed\nstdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("verify:"), "{stdout}");
}

#[test]
fn verify_rejects_a_pinned_source_that_no_longer_matches_the_bundle() {
    let fixture = published_bundle();
    let permalink_path = fixture.temp.path().join("match.permalink");
    let output = looklab()
        .arg("match")
        .arg("--look")
        .arg(&fixture.target)
        .arg("--bundle")
        .arg(&fixture.dir)
        .arg("--permalink")
        .arg(&permalink_path)
        .args(["--seed", "7", "--realization-version", "1"])
        .output()
        .expect("run");
    assert!(output.status.success(), "match failed");
    let observed = write_observed(&fixture);

    let text = std::fs::read_to_string(&permalink_path).expect("permalink");
    let mut permalink = looklab_match::Permalink::parse(&text).expect("parse");
    permalink.sources.base_look = Some("elsewhere.looklab".to_owned());
    std::fs::write(
        fixture.temp.path().join("elsewhere.looklab"),
        encode_document("elsewhere", &fixture.classic.model.base_look),
    )
    .expect("relocated base");
    std::fs::write(&permalink_path, permalink.canonical()).expect("tampered permalink");

    let output = run_verify(&permalink_path, &observed);
    let stderr = String::from_utf8(output.stderr).expect("utf-8 stderr");
    assert!(
        !output.status.success(),
        "verify accepted a relocated base: {stderr}"
    );
    assert!(
        stderr.contains("does not match the bundle content"),
        "{stderr}"
    );
}
