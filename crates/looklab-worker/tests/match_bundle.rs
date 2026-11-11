// SPDX-License-Identifier: AGPL-3.0-or-later

//! Camera bundle revisions publish independently of Look releases.

#![cfg(feature = "integration")]

use std::path::{Path, PathBuf};

use looklab_fit::synth::{LookSelector, SynthConfig, SyntheticDataset, truth_look};
use looklab_publish::{ArtifactEntry, MatchBundle};
use looklab_samples::{SampleSet, encode};
use looklab_schema::MatchBundleRevision;
use looklab_server::{ContributionId, CredentialId};
use looklab_upload::{BodyId, ManifestMetadata};
use looklab_worker::{JobPayload, Worker, WorkerConfig};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BODY: &str = "fujifilm/x-t5";
const FIRMWARE: &str = "1.50";
const EPOCH: u32 = 1;
const IMAGES: usize = 14;
const SAMPLES: usize = 1_200;
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
    config.snapshot_dir = config.data.join("community");
    config.site = site.clone();
    config.transforms = transforms;
    config.sweep_budget = None;
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
        look: Some(looklab_upload::LookSettings {
            white_balance: Some(looklab_upload::Tagged {
                raw: 0x0002,
                value: Some(looklab_upload::WhiteBalanceMode::Daylight),
            }),
            ..Default::default()
        }),
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
) {
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
}

async fn add_recipe_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    ordinal: u128,
    sim: &str,
    target: &str,
) {
    let id = ContributionId::try_from(
        uuid::Uuid::from_u128(0x1000_0000_0000_0000_0000_0000_0000_0000 + ordinal).to_string(),
    )
    .expect("contribution id");
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'recipe',
                 ?, NULL, ?, ?, ?, '[]', 'pending', 100)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(target)
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
}

async fn bundle_failure_state(worker: &Worker) -> String {
    let jobs: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT kind, status, attempts FROM jobs ORDER BY created_at, id")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let holds: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT subject_key, status, diagnostics FROM model_fit_holds ORDER BY subject_key",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let revisions: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT subject_key, status, numerical, release_gates, family, target_version
         FROM model_realization_revisions ORDER BY subject_key, version",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let heads: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT family, head_version, source_limited FROM model_family_heads ORDER BY family",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let eligibility: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.target, s.status
         FROM model_target_eligibility_current c
         JOIN model_target_eligibility_snapshots s ON s.id = c.snapshot_id
         ORDER BY c.target",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let pool_state: Vec<(String, String)> = sqlx::query_as(
        "SELECT subject_key, fingerprint FROM model_pool_state ORDER BY subject_key",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let artifacts: Vec<(String, String)> = sqlx::query_as(
        "SELECT owner_id, role FROM model_artifact_bindings
         WHERE owner_kind = 'realization_revision' ORDER BY owner_id, role",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let bundles: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM model_bundle_revisions")
        .fetch_one(worker.pool())
        .await
        .unwrap_or_default();
    format!(
        "jobs={jobs:?}\nholds={holds:?}\nrevisions={revisions:?}\nartifacts={artifacts:?}\nheads={heads:?}\neligibility={eligibility:?}\npool_state={pool_state:?}\nbundles={bundles}"
    )
}

async fn run_jobs(worker: &Worker) {
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");
}

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

fn bundle_dirs(site: &Path) -> Vec<PathBuf> {
    let root = site
        .join("data/bundles/fujifilm/x-t5")
        .join(EPOCH.to_string());
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.join("manifest.json").is_file())
        .collect();
    dirs.sort();
    dirs
}

fn verify_revision(dir: &Path) -> MatchBundleRevision {
    let bytes = std::fs::read(dir.join("manifest.json")).expect("manifest");
    let text = String::from_utf8(bytes.clone()).expect("utf-8");
    let manifest = MatchBundleRevision::parse_manifest(&text, 0).expect("manifest json");
    let digest = hex(&Sha256::digest(&bytes));
    assert_eq!(manifest.id.to_hex(), digest);
    assert_eq!(manifest.manifest_hash.to_hex(), digest);
    let artifacts: Vec<ArtifactEntry> =
        serde_json::from_slice(&std::fs::read(dir.join("artifacts.json")).expect("artifacts"))
            .expect("artifacts json");
    for entry in &artifacts {
        let file = std::fs::read(dir.join(entry.name.as_str())).expect("artifact file");
        assert_eq!(entry.bytes, file.len() as u64, "{}", entry.name);
        assert_eq!(entry.sha256, looklab_publish::ArtifactHash::of(&file));
    }
    for required in [
        "body.yaml",
        "transform.yaml",
        "body_params.json",
        "gauge.json",
    ] {
        assert!(
            artifacts
                .iter()
                .any(|entry| entry.name.as_str() == required),
            "{required}"
        );
    }
    let body_params = artifacts
        .iter()
        .find(|entry| entry.name.as_str() == "body_params.json")
        .expect("body params entry");
    let body_params_bytes = std::fs::read(dir.join("body_params.json")).expect("body params bytes");
    let params: looklab_schema::MatchParams =
        serde_json::from_slice(&body_params_bytes).expect("match params json");
    assert_eq!(params.stage, looklab_schema::MatchStage::Published);
    assert!(
        params
            .wb
            .iter()
            .any(|offset| offset.mode.as_str() == "daylight"),
        "published parameters omit the observed physical WB mode"
    );
    let body_params_hash = looklab_publish::ArtifactHash::of(&body_params_bytes);
    assert_eq!(body_params.sha256, body_params_hash);
    for sim in &manifest.sims {
        let params = sim.realization_params.expect("realization params pin");
        assert_eq!(
            sim.body_params.as_ref().map(|hash| hash.to_hex()),
            Some(body_params_hash.as_str().to_owned())
        );
        assert!(
            artifacts
                .iter()
                .any(|entry| entry.name.as_str() == params.to_hex()),
            "sim {} realization params",
            sim.native_sim
        );
    }
    manifest
}

fn verify_params(dir: &Path) {
    let bytes = std::fs::read(dir.join("manifest.json")).expect("manifest");
    let manifest =
        MatchBundleRevision::parse_manifest(&String::from_utf8(bytes).expect("utf-8"), 0)
            .expect("manifest json");
    for sim in &manifest.sims {
        let hash = sim.realization_params.expect("realization params pin");
        let params: looklab_schema::MatchRealizationParams = serde_json::from_slice(
            &std::fs::read(dir.join(hash.to_hex())).expect("realization params"),
        )
        .expect("realization params json");
        assert_eq!(params.family, sim.family_version);
        assert_eq!(
            looklab_schema::RealizationRevisionId::for_version(&params.realization),
            sim.realization
        );
    }
}

#[tokio::test]
async fn camera_bundles_publish_independently_of_target_releases() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let (classic, provia) = datasets();
    let credential = add_credential(worker, "seeder", 1.0).await;
    add_contribution(
        worker,
        &credential,
        &classic,
        &[0, 1, 2, 3],
        1,
        "classic_chrome",
    )
    .await;
    add_contribution(worker, &credential, &provia, &[0, 1, 2, 3], 2, "provia").await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![BodyId::new(BODY).expect("body")],
            },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue refit");
    run_jobs(worker).await;
    worker
        .enqueue(&JobPayload::PublishSite, 0)
        .await
        .expect("enqueue publish");
    run_jobs(worker).await;

    let dirs = bundle_dirs(&fixture.site);
    assert!(
        !dirs.is_empty(),
        "an accepted realization must publish an independent camera bundle\n{}",
        bundle_failure_state(worker).await
    );
    let first_ids: std::collections::BTreeSet<String> = dirs
        .iter()
        .filter_map(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    let manifests: Vec<MatchBundleRevision> = dirs.iter().map(|dir| verify_revision(dir)).collect();
    let heads_before: Vec<(String, i64)> =
        sqlx::query_as("SELECT family, head_version FROM model_family_heads ORDER BY family")
            .fetch_all(worker.pool())
            .await
            .expect("heads");
    let versions_before: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, version, status FROM look_versions ORDER BY look, version")
            .fetch_all(worker.pool())
            .await
            .expect("versions");
    assert!(
        !heads_before.is_empty(),
        "the first fit must establish a canonical family target"
    );
    assert!(
        manifests
            .iter()
            .flat_map(|manifest| &manifest.sims)
            .all(|sim| sim.realization_params.is_some())
    );
    for dir in &dirs {
        verify_params(dir);
    }

    add_recipe_contribution(
        worker,
        &credential,
        &classic,
        &[4, 5, 6, 7],
        3,
        "classic_chrome",
        "classic-chrome",
    )
    .await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![BodyId::new(BODY).expect("body")],
            },
            looklab_worker::clock::now_unix(),
        )
        .await
        .expect("enqueue second refit");
    run_jobs(worker).await;
    worker
        .enqueue(&JobPayload::PublishSite, 0)
        .await
        .expect("enqueue second publish");
    run_jobs(worker).await;

    let second = bundle_dirs(&fixture.site);
    let second_ids: std::collections::BTreeSet<String> = second
        .iter()
        .filter_map(|dir| {
            dir.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .collect();
    assert_ne!(
        second_ids, first_ids,
        "new camera evidence must publish a new bundle revision"
    );
    assert!(
        first_ids.iter().all(|id| second_ids.contains(id)),
        "old bundle revisions must remain resolvable"
    );
    for id in second_ids.difference(&first_ids) {
        let dir = second
            .iter()
            .find(|dir| dir.file_name().is_some_and(|name| name == id.as_str()))
            .expect("new bundle dir");
        verify_revision(dir);
        verify_params(dir);
    }
    let heads_after: Vec<(String, i64)> =
        sqlx::query_as("SELECT family, head_version FROM model_family_heads ORDER BY family")
            .fetch_all(worker.pool())
            .await
            .expect("heads");
    let versions_after: Vec<(String, i64, String)> =
        sqlx::query_as("SELECT look, version, status FROM look_versions ORDER BY look, version")
            .fetch_all(worker.pool())
            .await
            .expect("versions");
    assert_eq!(
        heads_after, heads_before,
        "camera update must not replace the family target"
    );
    assert_eq!(
        versions_after, versions_before,
        "camera update must not create a target release"
    );

    let legacy = MatchBundle::new(
        looklab_schema::BodyId::new(BODY).expect("body"),
        EPOCH,
        Vec::new(),
        Vec::new(),
    )
    .expect("v1 bundle");
    let decoded = MatchBundle::decode(&legacy.to_json().expect("json")).expect("v1 decode");
    assert_eq!(decoded.format, looklab_publish::MatchBundleFormat::V1);
}

#[tokio::test]
async fn an_unfitted_body_publishes_no_bundle_and_no_target_release() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    worker
        .enqueue(&JobPayload::PublishSite, 0)
        .await
        .expect("enqueue publish");
    run_jobs(worker).await;

    assert!(bundle_dirs(&fixture.site).is_empty());
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions")
        .fetch_one(worker.pool())
        .await
        .expect("versions");
    assert_eq!(versions, 0, "no target release without a fitted body state");
}
