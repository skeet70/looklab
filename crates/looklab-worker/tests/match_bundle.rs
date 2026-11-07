// SPDX-License-Identifier: AGPL-3.0-or-later

//! A release assembles one match bundle per body epoch of a matchable version,
//! and the published bundle ranks the sims the site can match to (docs/04 §2).

#![cfg(feature = "integration")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use looklab_fit::synth::{LookSelector, SynthConfig, SyntheticDataset, truth_look};
use looklab_match::{
    Emphasis, MatchModel, MatchOptions, Plan, PreparedTarget, TargetSpec, optimize,
};
use looklab_model::LookDocument;
use looklab_model::realization::decode_crm;
use looklab_publish::{
    MatchBundle, SimExclusion, VersionManifest, body_artifact, bundle_artifact, transform_artifact,
    verify_artifact,
};
use looklab_samples::{SampleSet, encode};
use looklab_server::admin;
use looklab_server::admin::ManifestArtifact;
use looklab_server::{ContributionId, CredentialId};
use looklab_upload::{BodyId, ManifestMetadata};
use looklab_worker::{JobPayload, Worker, WorkerConfig};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BODY: &str = "fujifilm/x-t5";
const FIRMWARE: &str = "1.50";
const EPOCH: u32 = 1;

/// Jobs one fixture queue may run before it must settle.
const DRAIN_JOBS: usize = 64;
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);

const TRANSFORM: &str = "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
";

struct Fixture {
    _dir: TempDir,
    worker: Worker,
    site: PathBuf,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let db = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    let site = dir.path().join("site/data");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let transforms = dir.path().join("transforms");
    std::fs::create_dir_all(&transforms).expect("transform dir");
    std::fs::write(transforms.join("fujifilm-x-t5.yaml"), TRANSFORM).expect("transform file");
    let mut config = WorkerConfig::new(&db, &blobs);
    config.data = data;
    config.site = site.clone();
    config.transforms = transforms;
    config.sweep_budget = Some(8_000_000.0);
    let worker = Worker::open(config).await.expect("worker");
    Fixture {
        _dir: dir,
        worker,
        site,
    }
}

async fn add_credential(worker: &Worker, tier: &str, reputation: f64) -> CredentialId {
    let id = CredentialId::try_from(
        uuid::Uuid::from_u128(0x3000_0000_0000_0000_0000_0000_0000_0001).to_string(),
    )
    .expect("credential id");
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', ?, '[]', ?, 0, 'active')",
    )
    .bind(id.as_str())
    .bind(id.as_str().as_bytes())
    .bind(tier)
    .bind(reputation)
    .execute(worker.pool())
    .await
    .expect("credential");
    id
}

fn metadata(index: usize, ordinal: u128) -> ManifestMetadata {
    ManifestMetadata {
        make: Some("Fujifilm".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some(FIRMWARE.to_owned()),
        lens_model: Some("XF 35mm F1.4 R".to_owned()),
        focal_length_mm: Some(35.0),
        iso: Some(200),
        shutter_seconds: Some(1.0 / (index as f64 + 1.0 + 100.0 * ordinal as f64)),
        aperture_f_number: Some(2.0),
        exposure_compensation_ev: Some(0.0),
        as_shot_wb: None,
        capture_month: None,
        software: Some("looklab-test".to_owned()),
        render_tag: None,
        render_tag_malformed: false,
        look: None,
    }
}

fn identity_camera_blob(dataset: &SyntheticDataset, index: usize) -> Vec<u8> {
    let samples = &dataset.images[index].samples;
    let in_rgb: Vec<[f32; 3]> = dataset.points[index]
        .iter()
        .map(|xyz| [xyz.x as f32, xyz.y as f32, xyz.z as f32])
        .collect();
    let set = SampleSet::new(
        samples.header().clone(),
        in_rgb,
        samples.out_rgb().to_vec(),
        samples.density_w().to_vec(),
        samples.flags().to_vec(),
    )
    .expect("sample set");
    encode(&set).expect("encode")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn add_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    ordinal: u128,
    sim: &str,
) -> ContributionId {
    let id = ContributionId::try_from(
        uuid::Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0000 + ordinal).to_string(),
    )
    .expect("contribution id");
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native',
                 NULL, NULL, ?, ?, ?, '[]', 'pending', 100)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(BODY)
    .bind(FIRMWARE)
    .bind(sim)
    .execute(worker.pool())
    .await
    .expect("contribution");
    for &index in indices {
        let bytes = identity_camera_blob(dataset, index);
        let digest = Sha256::digest(&bytes);
        let hash: [u8; 32] = digest.into();
        std::fs::write(worker.config().blobs.join(hex(&hash)), &bytes).expect("blob file");
        sqlx::query(
            "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        )
        .bind(hash.as_slice())
        .execute(worker.pool())
        .await
        .expect("blob row");
        let image_id = uuid::Uuid::from_u128(
            0x2000_0000_0000_0000_0000_0000_0000_0000 + (ordinal << 8) + index as u128,
        )
        .to_string();
        let meta = serde_json::to_string(&metadata(index, ordinal)).expect("metadata");
        sqlx::query(
            "INSERT INTO contribution_images
             (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status)
             VALUES (?, ?, ?, ?, ?, NULL, 'pending')",
        )
        .bind(&image_id)
        .bind(id.as_str())
        .bind(hash.as_slice())
        .bind(i64::try_from(dataset.images[index].samples.n()).expect("n"))
        .bind(meta)
        .execute(worker.pool())
        .await
        .expect("image");
    }
    id
}

async fn run_jobs(worker: &Worker) {
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");
}

fn version_dir(site: &Path, look: &str) -> PathBuf {
    version_path(site, look, "1.0.0")
}

fn version_path(site: &Path, look: &str, version: &str) -> PathBuf {
    site.join(format!("data/looks/{look}/{version}"))
}

fn load_bundle(dir: &Path) -> MatchBundle {
    let path = dir.join(
        bundle_artifact(&BodyId::new(BODY).expect("body"), EPOCH)
            .expect("descriptor")
            .as_str(),
    );
    MatchBundle::decode(&std::fs::read(path).expect("descriptor")).expect("bundle")
}

fn load_manifest(dir: &Path) -> VersionManifest {
    serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).expect("manifest"))
        .expect("manifest json")
}

async fn state_report(worker: &Worker) -> String {
    let versions: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, version, status FROM look_versions ORDER BY look, version")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let attempts: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, consecutive, states FROM fit_attempts ORDER BY look")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let attempts: Vec<String> = attempts
        .into_iter()
        .map(|(look, consecutive, states)| {
            let resume = serde_json::from_str::<BTreeMap<String, String>>(&states)
                .ok()
                .and_then(|mut states| states.remove(BODY))
                .and_then(|state| serde_json::from_str::<looklab_fit::FitState>(&state).ok())
                .map(|state| {
                    format!(
                        "stage {} sweep {} converged {}",
                        state.stage, state.sweep, state.converged
                    )
                });
            format!("{look} consecutive {consecutive} resume {resume:?}")
        })
        .collect();
    let holds: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, consecutive, diagnostics FROM fit_holds ORDER BY look")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let gates: Vec<(String, i64)> =
        sqlx::query_as("SELECT look, version FROM gate_failures ORDER BY look, version")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let jobs: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT kind, status, attempts FROM jobs ORDER BY created_at, id")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let publication: Option<(i64, i64)> =
        sqlx::query_as("SELECT generation, published FROM publication WHERE id = 1")
            .fetch_optional(worker.pool())
            .await
            .unwrap_or_default();
    format!(
        "versions={versions:?} attempts={attempts:?} holds={holds:?} gate_failures={gates:?} jobs={jobs:?} publication={publication:?}"
    )
}

async fn assert_default_releases(worker: &Worker) {
    let published: Vec<String> = sqlx::query_scalar(
        "SELECT look FROM look_versions WHERE status IN ('published', 'rooted') ORDER BY look",
    )
    .fetch_all(worker.pool())
    .await
    .expect("versions");
    assert_eq!(
        published,
        ["classic-chrome", "provia"],
        "{}",
        state_report(worker).await
    );
}

fn scalar_stats(scalars: Option<&looklab_model::ProbeScalars>) -> String {
    let Some(scalars) = scalars else {
        return "none".to_owned();
    };
    let mut values = scalars.values.clone();
    values.sort_by(f64::total_cmp);
    format!(
        "min {:.3} median {:.3} max {:.3}",
        values.first().copied().unwrap_or(f64::NAN),
        values.get(values.len() / 2).copied().unwrap_or(f64::NAN),
        values.last().copied().unwrap_or(f64::NAN)
    )
}

fn document_report(document: &LookDocument) -> String {
    let supported = document
        .uncertainty
        .support
        .as_ref()
        .map(|support| support.values.iter().filter(|value| **value > 0.5).count())
        .unwrap_or(0);
    let total = document
        .uncertainty
        .support
        .as_ref()
        .map_or(0, |support| support.values.len());
    let density_max = document
        .uncertainty
        .density
        .as_ref()
        .and_then(|density| density.values.iter().copied().reduce(f64::max))
        .unwrap_or(f64::NAN);
    format!(
        "matchable={} refusal={:?} supported={supported}/{total} sigma_post[{}] bias[{}] density_max={density_max:.4}",
        document.is_matchable(),
        document.match_refusal(),
        scalar_stats(document.uncertainty.probe_sigma_post.as_ref()),
        scalar_stats(document.uncertainty.probe_bias.as_ref()),
    )
}

async fn weight_report(worker: &Worker, look: &str) -> String {
    let body = BodyId::new(BODY).expect("body");
    let look = looklab_upload::LookSlug::new(look).expect("look");
    let Ok(inspection) =
        looklab_worker::weights::inspect(worker.pool(), worker.config(), &body, &look).await
    else {
        return "weights unavailable".to_owned();
    };
    let mut weights: Vec<f64> = inspection
        .images
        .iter()
        .map(|image| image.effective_look_weight)
        .collect();
    weights.sort_by(f64::total_cmp);
    let mut trust: Vec<f32> = inspection
        .images
        .iter()
        .flat_map(|image| image.sample_trust.iter().copied())
        .collect();
    trust.sort_by(f32::total_cmp);
    format!(
        "images={} effective_weight[min {:.3} median {:.3} max {:.3}] sample_trust[min {:.3} median {:.3} max {:.3}]",
        inspection.images.len(),
        weights.first().copied().unwrap_or(f64::NAN),
        weights.get(weights.len() / 2).copied().unwrap_or(f64::NAN),
        weights.last().copied().unwrap_or(f64::NAN),
        trust.first().copied().unwrap_or(f32::NAN),
        trust.get(trust.len() / 2).copied().unwrap_or(f32::NAN),
        trust.last().copied().unwrap_or(f32::NAN),
    )
}

async fn assert_matchable(worker: &Worker, site: &Path, look: &str, version: &str) {
    let dir = version_path(site, look, version);
    let document = LookDocument::decode(
        &std::fs::read(dir.join("look.looklab")).expect("published look document"),
    )
    .expect("look document");
    assert!(
        document.is_matchable(),
        "{} weights={}",
        document_report(&document),
        weight_report(worker, look).await
    );
}

fn stats(values: &[f64]) -> String {
    let mut values = values.to_vec();
    values.sort_by(f64::total_cmp);
    format!(
        "min {:.3} median {:.3} max {:.3}",
        values.first().copied().unwrap_or(f64::NAN),
        values.get(values.len() / 2).copied().unwrap_or(f64::NAN),
        values.last().copied().unwrap_or(f64::NAN)
    )
}

async fn stored_probe_reports(worker: &Worker, look: &str) -> String {
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM look_versions WHERE look = ? ORDER BY version")
            .bind(look)
            .fetch_all(worker.pool())
            .await
            .expect("versions");
    let mut reports = Vec::new();
    for version in versions {
        let Some(artifacts) = looklab_worker::db::manifest_artifacts(
            worker.pool(),
            look,
            u32::try_from(version).unwrap_or(u32::MAX),
        )
        .await
        .expect("manifest") else {
            continue;
        };
        let Some(entry) = artifacts
            .iter()
            .find(|artifact| artifact.name == "release.json")
        else {
            continue;
        };
        let bytes = std::fs::read(worker.config().blobs.join(&entry.sha256)).expect("release blob");
        let record: looklab_worker::ReleaseRecord =
            serde_json::from_slice(&bytes).expect("release record");
        let probes = &record.probes;
        let supported = probes
            .iter()
            .filter(|probe| probe.support.supported())
            .count();
        let sigma = probes.iter().filter(|probe| probe.support.sigma).count();
        let density = probes.iter().filter(|probe| probe.support.density).count();
        let clusters = probes.iter().filter(|probe| probe.support.clusters).count();
        let bias = probes.iter().filter(|probe| probe.support.bias).count();
        let sigma_post: Vec<f64> = probes.iter().map(|probe| probe.sigma_post).collect();
        let bias_de: Vec<f64> = probes.iter().map(|probe| probe.bias_de).collect();
        reports.push(format!(
            "{look}@{version} gauge={} convention={:?} anchor={} probes={} supported={supported} causes sigma={sigma} density={density} clusters={clusters} bias={bias} sigma_post[{}] bias[{}]",
            record.gauge,
            record.gauge_convention,
            record.anchor,
            probes.len(),
            stats(&sigma_post),
            stats(&bias_de),
        ));
    }
    reports.join("\n")
}

async fn gate_failure_reports(worker: &Worker) -> String {
    let rows: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, version, report FROM gate_failures ORDER BY created_at, id")
            .fetch_all(worker.pool())
            .await
            .expect("gate failures");
    rows.into_iter()
        .map(|(look, version, report)| {
            let report = serde_json::from_str::<serde_json::Value>(&report)
                .ok()
                .and_then(|value| serde_json::to_string(&value).ok())
                .unwrap_or(report);
            format!("{look}@{version}: {report}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn assert_look_versions(worker: &Worker, look: &str, expected: &[i64]) {
    let versions: Vec<i64> =
        sqlx::query_scalar("SELECT version FROM look_versions WHERE look = ? ORDER BY version")
            .bind(look)
            .fetch_all(worker.pool())
            .await
            .expect("versions");
    assert_eq!(
        versions,
        expected,
        "{}\nweights={}\nstored_probes:\n{}\ngate_failures:\n{}",
        state_report(worker).await,
        weight_report(worker, look).await,
        stored_probe_reports(worker, look).await,
        gate_failure_reports(worker).await
    );
}

async fn assert_db_matchable(worker: &Worker, look: &str, version: u32) {
    let artifacts = looklab_worker::db::manifest_artifacts(worker.pool(), look, version)
        .await
        .expect("manifest")
        .expect("manifest row");
    let entry = artifacts
        .iter()
        .find(|artifact| artifact.name == "look.looklab")
        .expect("look artifact");
    let bytes = std::fs::read(worker.config().blobs.join(&entry.sha256)).expect("look blob");
    let document = LookDocument::decode(&bytes).expect("look document");
    assert!(
        document.is_matchable(),
        "{} weights={}",
        document_report(&document),
        weight_report(worker, look).await
    );
}

fn verify_bundle_artifacts(dir: &Path, bundle: &MatchBundle) {
    let manifest = load_manifest(dir);
    for sim in &bundle.sims {
        let base = std::fs::read(dir.join(sim.base.as_str())).expect("base bytes");
        let crm_bytes = std::fs::read(dir.join(sim.crm.as_str())).expect("crm bytes");
        verify_artifact(&manifest, &sim.base, &base).expect("base hash");
        verify_artifact(&manifest, &sim.crm, &crm_bytes).expect("crm hash");
        LookDocument::decode(&base).expect("base document");
        let crm = decode_crm(&crm_bytes).expect("crm");
        assert_eq!(crm.body.as_str(), BODY);
        assert_eq!(crm.epoch, EPOCH);
        assert_eq!(&crm.base_sim, &sim.sim);
    }
    let body_name = body_artifact(&bundle.body, EPOCH).expect("body name");
    let body = std::fs::read(dir.join(body_name.as_str())).expect("body schema");
    verify_artifact(&manifest, &body_name, &body).expect("body hash");
    let schema =
        looklab_schema::validate_control_schema(std::str::from_utf8(&body).expect("schema utf-8"))
            .map_err(|errors| format!("{errors:?}"))
            .expect("schema");
    assert_eq!(schema.body.as_str(), BODY);
    let transform_name = transform_artifact(&bundle.body, EPOCH).expect("transform name");
    let transform = std::fs::read(dir.join(transform_name.as_str())).expect("transform");
    verify_artifact(&manifest, &transform_name, &transform).expect("transform hash");
}

const IMAGES: usize = 14;
const SAMPLES: usize = 1_200;

/// The worker supplies a zero relative exposure hint, so the synthetic
/// fixture carries no exposure or hint nuisance.
fn fixture_config(seed: u64) -> SynthConfig {
    SynthConfig {
        seed,
        images: IMAGES,
        samples_per_image: SAMPLES,
        exposure_sd: 0.0,
        hint_sd: 0.0,
        ..SynthConfig::default()
    }
}

/// Two native Looks rendered through different truths.
fn datasets() -> (SyntheticDataset, SyntheticDataset) {
    let classic = looklab_fit::synth::dataset(&fixture_config(21));
    let truth = truth_look(33);
    let provia = looklab_fit::synth::dataset_with(
        &fixture_config(33),
        &truth,
        &LookSelector::Single(&truth),
    );
    (classic, provia)
}

/// The winning sim and median ΔE the published bundle predicts for `target`.
fn rank(dir: &Path, target: &LookDocument) -> (String, f64) {
    let bundle = load_bundle(dir);
    let body = body_artifact(&bundle.body, EPOCH).expect("body name");
    let schema = looklab_schema::validate_control_schema(
        std::str::from_utf8(&std::fs::read(dir.join(body.as_str())).expect("schema bytes"))
            .expect("schema utf-8"),
    )
    .map_err(|errors| format!("{errors:?}"))
    .expect("schema");
    #[derive(serde::Deserialize)]
    struct Transform {
        version: u32,
        camera_to_xyz: looklab_color::Mat3,
    }
    let transform_name = transform_artifact(&bundle.body, EPOCH).expect("transform name");
    let transform: Transform = serde_norway::from_str(
        std::str::from_utf8(
            &std::fs::read(dir.join(transform_name.as_str())).expect("transform bytes"),
        )
        .expect("transform utf-8"),
    )
    .expect("transform yaml");
    let mut models = Vec::new();
    for sim in &bundle.sims {
        let base =
            LookDocument::decode(&std::fs::read(dir.join(sim.base.as_str())).expect("base bytes"))
                .expect("base document");
        let crm = decode_crm(&std::fs::read(dir.join(sim.crm.as_str())).expect("crm bytes"))
            .expect("crm");
        models.push(
            MatchModel::new(
                base.look.clone(),
                crm,
                transform.camera_to_xyz,
                None,
                transform.version,
                0,
            )
            .expect("model"),
        );
    }
    let probes = looklab_model::probes::v1();
    let prepared = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target.look,
        uncertainty: Some(&target.uncertainty),
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
    })
    .expect("target");
    let plan = Plan::new(&schema, EPOCH).expect("plan");
    let outcome = optimize(
        &models,
        &prepared,
        &plan,
        &MatchOptions {
            seed: 7,
            ..MatchOptions::default()
        },
    )
    .expect("optimize");
    assert_eq!(outcome.stats.sims_considered, bundle.sims.len());
    (
        outcome.results[0].sim.as_str().to_owned(),
        outcome.results[0].median_delta_e,
    )
}

#[cfg(feature = "full-scale")]
#[test]
#[ignore = "fixture convergence diagnostic; run with --ignored --nocapture"]
fn classic_fixture_controlled_fit_converges() {
    let data = looklab_fit::synth::dataset(&fixture_config(21));
    let schema = looklab_schema::validate_control_schema(
        &std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t5.yaml"),
        )
        .expect("schema file"),
    )
    .expect("schema");
    let options = looklab_fit::FitOptions {
        seed: 0x5eed_1a5e,
        max_samples_per_image: 10_000,
        sweep_budget: 8_000_000.0,
        ..looklab_fit::FitOptions::default()
    };
    let (result, resume) = looklab_fit::fit_controlled_with_history_resumable(
        &data.images,
        &schema,
        EPOCH,
        &looklab_schema::SimKey::new("classic_chrome").expect("sim"),
        &options,
        None,
        None,
    )
    .expect("controlled fit");
    let supported = result
        .probes
        .probes
        .iter()
        .filter(|probe| probe.supported)
        .count();
    let sigma = result
        .probes
        .probes
        .iter()
        .filter(|probe| probe.supported_sigma)
        .count();
    let density = result
        .probes
        .probes
        .iter()
        .filter(|probe| probe.supported_density)
        .count();
    let clusters = result
        .probes
        .probes
        .iter()
        .filter(|probe| probe.supported_clusters)
        .count();
    let bias = result
        .probes
        .probes
        .iter()
        .filter(|probe| probe.supported_bias)
        .count();
    let sigma_post_max = result
        .probes
        .probes
        .iter()
        .map(|probe| probe.post_sigma_de)
        .fold(0.0_f64, f64::max);
    let bias_max = result
        .probes
        .probes
        .iter()
        .map(|probe| probe.bias_de)
        .fold(0.0_f64, f64::max);
    let density_max = result
        .probes
        .probes
        .iter()
        .map(|probe| probe.density)
        .fold(0.0_f64, f64::max);
    eprintln!(
        "converged={} sweeps={} evaluations={} supported={}/{} causes sigma={sigma} density={density} clusters={clusters} bias={bias} sigma_post_max={sigma_post_max:.3} bias_max={bias_max:.3} density_max={density_max:.4} resume={resume:?}",
        result.diagnostics.converged,
        result.diagnostics.sweeps,
        result.diagnostics.evaluations,
        supported,
        result.probes.probes.len()
    );
    assert!(result.diagnostics.converged, "{resume:?}");
}

#[tokio::test]
async fn publish_writes_a_bundle_with_the_sims_published_at_each_release() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let (classic, provia) = datasets();
    let credential = add_credential(worker, "seeder", 1.5).await;
    let all: Vec<usize> = (0..IMAGES).collect();
    add_contribution(worker, &credential, &classic, &all, 1, "classic_chrome").await;
    add_contribution(worker, &credential, &provia, &all, 2, "provia").await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;

    assert_default_releases(worker).await;
    assert_matchable(worker, &fixture.site, "classic-chrome", "1.0.0").await;
    assert_matchable(worker, &fixture.site, "provia", "1.0.0").await;

    let classic_dir = version_dir(&fixture.site, "classic-chrome");
    let classic = load_bundle(&classic_dir);
    assert_eq!(classic.body.as_str(), BODY);
    assert_eq!(classic.epoch, EPOCH);
    let ranked: Vec<&str> = classic.sims.iter().map(|sim| sim.sim.as_str()).collect();
    assert_eq!(ranked, ["classic_chrome"]);
    assert_eq!(classic.excluded.len(), 1);
    assert_eq!(classic.excluded[0].sim.as_str(), "provia");
    assert_eq!(classic.excluded[0].reason, SimExclusion::NoPublishedLook);
    verify_bundle_artifacts(&classic_dir, &classic);

    let provia_dir = version_dir(&fixture.site, "provia");
    let provia = load_bundle(&provia_dir);
    assert_eq!(provia.body.as_str(), BODY);
    assert_eq!(provia.epoch, EPOCH);
    let ranked: Vec<&str> = provia.sims.iter().map(|sim| sim.sim.as_str()).collect();
    assert_eq!(ranked, ["classic_chrome", "provia"]);
    assert!(provia.excluded.is_empty(), "{:?}", provia.excluded);
    verify_bundle_artifacts(&provia_dir, &provia);

    for target in ["classic-chrome", "provia"] {
        let dir = version_dir(&fixture.site, target);
        let target_document = LookDocument::decode(
            &std::fs::read(dir.join("look.looklab")).expect("target document"),
        )
        .expect("target document");
        let (sim, median) = rank(&dir, &target_document);
        let expected = target.replace('-', "_");
        assert_eq!(sim, expected, "{target}");
        assert!(median < 0.05, "{target}: {median}");
    }
}

#[tokio::test]
async fn a_hidden_look_leaves_its_sims_out_of_the_bundle() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let (classic, provia) = datasets();
    let credential = add_credential(worker, "seeder", 1.5).await;
    let all: Vec<usize> = (0..IMAGES).collect();
    add_contribution(worker, &credential, &classic, &all, 1, "classic_chrome").await;
    add_contribution(worker, &credential, &provia, &all, 2, "provia").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;
    assert_default_releases(worker).await;
    assert_matchable(worker, &fixture.site, "classic-chrome", "1.0.0").await;
    assert_matchable(worker, &fixture.site, "provia", "1.0.0").await;

    admin::hide_look(
        worker.pool(),
        &credential,
        &looklab_upload::LookSlug::new("provia").expect("slug"),
        "test",
    )
    .await
    .expect("hide");
    run_jobs(worker).await;
    let refreshed = looklab_fit::synth::dataset_with(
        &fixture_config(22),
        &classic.truth,
        &LookSelector::Single(&classic.truth),
    );
    add_contribution(worker, &credential, &refreshed, &all, 3, "classic_chrome").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;
    assert_look_versions(worker, "classic-chrome", &[1, 2]).await;
    assert_db_matchable(worker, "classic-chrome", 2).await;
    assert!(
        version_path(&fixture.site, "classic-chrome", "2.0.0").exists(),
        "{}",
        state_report(worker).await
    );

    let bundle = load_bundle(&version_path(&fixture.site, "classic-chrome", "2.0.0"));
    let ranked: Vec<&str> = bundle.sims.iter().map(|sim| sim.sim.as_str()).collect();
    assert_eq!(ranked, ["classic_chrome"]);
    assert_eq!(bundle.excluded.len(), 1);
    assert_eq!(bundle.excluded[0].sim.as_str(), "provia");
    assert_eq!(bundle.excluded[0].reason, SimExclusion::NoPublishedLook);
    assert!(!fixture.site.join("data/looks/provia").exists());
}

#[tokio::test]
async fn a_newer_source_sim_leaves_an_old_bundle_and_manifest_unchanged() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let (classic, provia) = datasets();
    let credential = add_credential(worker, "seeder", 1.5).await;
    let all: Vec<usize> = (0..IMAGES).collect();
    add_contribution(worker, &credential, &classic, &all, 1, "classic_chrome").await;
    add_contribution(worker, &credential, &provia, &all, 2, "provia").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;
    assert_default_releases(worker).await;
    assert_matchable(worker, &fixture.site, "classic-chrome", "1.0.0").await;
    assert_matchable(worker, &fixture.site, "provia", "1.0.0").await;

    let dir = version_dir(&fixture.site, "provia");
    let manifest_before = std::fs::read(dir.join("manifest.json")).expect("manifest");
    let bundle = load_bundle(&dir);
    let bundle_name = bundle_artifact(&bundle.body, EPOCH).expect("descriptor");
    let bundle_before = std::fs::read(dir.join(bundle_name.as_str())).expect("bundle");
    let mut artifacts_before = Vec::new();
    for sim in &bundle.sims {
        for name in [&sim.base, &sim.crm] {
            artifacts_before.push((
                name.as_str().to_owned(),
                std::fs::read(dir.join(name.as_str())).expect("bundle artifact"),
            ));
        }
    }
    let manifest_row_before: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = 'provia' AND v.version = 1",
    )
    .fetch_one(worker.pool())
    .await
    .expect("stored manifest");

    let truth = truth_look(33);
    let refreshed = looklab_fit::synth::dataset_with(
        &fixture_config(41),
        &truth,
        &LookSelector::Single(&truth),
    );
    add_contribution(worker, &credential, &refreshed, &all, 3, "provia").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;

    assert_look_versions(worker, "provia", &[1, 2]).await;
    assert_db_matchable(worker, "provia", 2).await;
    assert!(
        version_path(&fixture.site, "provia", "2.0.0").exists(),
        "{}",
        state_report(worker).await
    );

    assert_eq!(
        std::fs::read(dir.join("manifest.json")).expect("manifest"),
        manifest_before
    );
    assert_eq!(
        std::fs::read(dir.join(bundle_name.as_str())).expect("bundle"),
        bundle_before
    );
    for (name, bytes) in &artifacts_before {
        assert_eq!(
            &std::fs::read(dir.join(name)).expect("bundle artifact"),
            bytes,
            "{name}"
        );
    }
    let manifest_row_after: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = 'provia' AND v.version = 1",
    )
    .fetch_one(worker.pool())
    .await
    .expect("stored manifest");
    assert_eq!(manifest_row_after, manifest_row_before);
}

#[tokio::test]
async fn a_hidden_targets_release_keeps_its_bundle_for_relisting() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let (classic, provia) = datasets();
    let credential = add_credential(worker, "seeder", 1.5).await;
    let all: Vec<usize> = (0..IMAGES).collect();
    add_contribution(worker, &credential, &classic, &all, 1, "classic_chrome").await;
    add_contribution(worker, &credential, &provia, &all, 2, "provia").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;
    assert_default_releases(worker).await;
    assert_matchable(worker, &fixture.site, "classic-chrome", "1.0.0").await;
    assert_matchable(worker, &fixture.site, "provia", "1.0.0").await;

    let look = looklab_upload::LookSlug::new("classic-chrome").expect("slug");
    admin::hide_look(worker.pool(), &credential, &look, "test")
        .await
        .expect("hide");
    run_jobs(worker).await;
    let refreshed = looklab_fit::synth::dataset_with(
        &fixture_config(22),
        &classic.truth,
        &LookSelector::Single(&classic.truth),
    );
    add_contribution(worker, &credential, &refreshed, &all, 3, "classic_chrome").await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit { bodies: Vec::new() },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue");
    run_jobs(worker).await;
    assert_look_versions(worker, "classic-chrome", &[1, 2]).await;
    assert_db_matchable(worker, "classic-chrome", 2).await;

    assert!(!fixture.site.join("data/looks/classic-chrome").exists());
    let stored: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = 'classic-chrome' AND v.version = 2",
    )
    .fetch_one(worker.pool())
    .await
    .expect("stored manifest");
    let artifacts: Vec<ManifestArtifact> = serde_json::from_str(&stored).expect("artifacts");
    let body = BodyId::new(BODY).expect("body");
    let descriptor_name = bundle_artifact(&body, EPOCH).expect("descriptor");
    let descriptor = artifacts
        .iter()
        .find(|artifact| artifact.name.as_str() == descriptor_name.as_str())
        .expect("descriptor entry");
    let descriptor_bytes =
        std::fs::read(worker.config().blobs.join(&descriptor.sha256)).expect("descriptor blob");
    assert_eq!(hex(&Sha256::digest(&descriptor_bytes)), descriptor.sha256);
    let original = MatchBundle::decode(&descriptor_bytes).expect("bundle");
    assert!(
        original
            .sims
            .iter()
            .any(|sim| sim.sim.as_str() == "classic_chrome")
    );
    for sim in &original.sims {
        for name in [&sim.base, &sim.crm] {
            let entry = artifacts
                .iter()
                .find(|artifact| artifact.name.as_str() == name.as_str())
                .expect("artifact entry");
            let bytes =
                std::fs::read(worker.config().blobs.join(&entry.sha256)).expect("artifact blob");
            assert_eq!(hex(&Sha256::digest(&bytes)), entry.sha256);
        }
    }

    admin::relist_look(worker.pool(), &credential, &look, "test")
        .await
        .expect("relist");
    run_jobs(worker).await;

    let dir = version_path(&fixture.site, "classic-chrome", "2.0.0");
    let published =
        std::fs::read(dir.join(descriptor_name.as_str())).expect("published descriptor");
    assert_eq!(published, descriptor_bytes);
    let manifest = load_manifest(&dir);
    verify_artifact(&manifest, &descriptor_name, &published).expect("descriptor hash");
    MatchBundle::decode(&published).expect("published bundle");
    let after: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = 'classic-chrome' AND v.version = 2",
    )
    .fetch_one(worker.pool())
    .await
    .expect("stored manifest");
    assert_eq!(after, stored);
}
