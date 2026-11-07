// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;
#[cfg(feature = "integration")]
use std::path::PathBuf;

#[cfg(feature = "integration")]
use looklab_export::card::read_card;
#[cfg(feature = "integration")]
use looklab_fit::synth::LookSelector;
use looklab_fit::synth::{SynthConfig, SyntheticDataset};
use looklab_model::LookDocument;
#[cfg(feature = "integration")]
use looklab_publish::{ArtifactName, CARD_ARTIFACT, SiteIndex, VersionManifest, verify_artifact};
use looklab_samples::{SampleSet, encode};
use looklab_server::{ContributionId, CredentialId};
#[cfg(feature = "integration")]
use looklab_upload::CardDeclaration;
use looklab_upload::{ContributionKind, ManifestMetadata, RenderTag};
use looklab_worker::db::ContributionIntent;
#[cfg(feature = "integration")]
use looklab_worker::db::{ContributionStatus, ImageOutcome};
use looklab_worker::{JobPayload, Worker, WorkerConfig, WorkerError};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BODY: &str = "fujifilm/x-t5";
const LOOK: &str = "classic-chrome";
const FIRMWARE: &str = "1.50";
const SIM: &str = "classic_chrome";

struct Fixture {
    _dir: Option<TempDir>,
    worker: Worker,
    #[cfg(feature = "integration")]
    site: PathBuf,
}

const TRANSFORM: &str = "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
";

async fn fixture() -> Fixture {
    fixture_with_transform(true).await
}

async fn fixture_with_transform(write_transform: bool) -> Fixture {
    fixture_with(write_transform, |_| {}).await
}

async fn fixture_with(write_transform: bool, configure: impl FnOnce(&mut WorkerConfig)) -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let db = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let transforms = dir.path().join("transforms");
    std::fs::create_dir_all(&transforms).expect("transform dir");
    if write_transform {
        std::fs::write(transforms.join("fujifilm-x-t5.yaml"), TRANSFORM).expect("transform file");
    }
    let mut config = WorkerConfig::new(&db, &blobs);
    config.data = data;
    config.site = dir.path().join("site/data");
    config.transforms = transforms;
    configure(&mut config);
    let worker = Worker::open(config).await.expect("worker");
    if std::env::var_os("LOOKLAB_KEEP_FIXTURE").is_some() {
        eprintln!("fixture kept at {}", dir.path().display());
        std::mem::forget(dir);
        return Fixture {
            _dir: None,
            #[cfg(feature = "integration")]
            site: worker.config().site.clone(),
            worker,
        };
    }
    Fixture {
        _dir: Some(dir),
        #[cfg(feature = "integration")]
        site: worker.config().site.clone(),
        worker,
    }
}

async fn add_credential(worker: &Worker, tier: &str, reputation: f64) -> CredentialId {
    let id = CredentialId::new();
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

fn metadata(index: usize) -> ManifestMetadata {
    ManifestMetadata {
        make: Some("Fujifilm".to_owned()),
        model: Some("X-T5".to_owned()),
        firmware: Some(FIRMWARE.to_owned()),
        lens_model: Some("XF 35mm F1.4 R".to_owned()),
        focal_length_mm: Some(35.0),
        iso: Some(200),
        shutter_seconds: Some(1.0 / (index as f64 + 1.0)),
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

fn blob_for(dataset: &SyntheticDataset, index: usize) -> Vec<u8> {
    let samples = &dataset.images[index].samples;
    let nuisance = dataset.nuisances[index];
    let gain = libm::exp2(nuisance.exposure);
    let inverse_gains = [
        libm::exp(-nuisance.wb_log_r) / gain,
        1.0 / gain,
        libm::exp(-nuisance.wb_log_b) / gain,
    ];
    let in_rgb: Vec<[f32; 3]> = dataset.points[index]
        .iter()
        .map(|xyz| {
            [
                (xyz.x * inverse_gains[0]) as f32,
                (xyz.y * inverse_gains[1]) as f32,
                (xyz.z * inverse_gains[2]) as f32,
            ]
        })
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

#[cfg(feature = "integration")]
/// A mislabel no per-image nuisance can absorb.
fn mislabelled_look(truth: &looklab_model::Look) -> looklab_model::Look {
    let mut look = looklab_fit::synth::perturbed_look(truth);
    for value in &mut look.tint.a {
        *value += 6.0;
    }
    for value in &mut look.color.g.volume {
        *value += 0.35;
    }
    for value in &mut look.color.r.volume {
        *value += 20.0;
    }
    look
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[allow(clippy::too_many_arguments)]
async fn add_contribution_with(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
    kind: ContributionKind,
    intent: ContributionIntent,
    target_look: Option<&str>,
    settings: &str,
) -> (ContributionId, Vec<String>) {
    insert_contribution(
        worker,
        credential,
        dataset,
        indices,
        created_at,
        kind,
        intent,
        target_look,
        settings,
        None,
        None,
    )
    .await
}

#[cfg(feature = "integration")]
async fn add_card_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
    declaration: &CardDeclaration,
) -> (ContributionId, Vec<String>) {
    let card = serde_json::to_string(declaration).expect("card declaration");
    let tag = match declaration {
        CardDeclaration::Tagged(tag) => Some(tag),
        CardDeclaration::UserDeclared { .. } => None,
    };
    insert_contribution(
        worker,
        credential,
        dataset,
        indices,
        created_at,
        ContributionKind::Rerender,
        ContributionIntent::Card,
        None,
        "[]",
        Some(&card),
        tag,
    )
    .await
}

/// The `contributions.kind` column text for a client-claimed kind.
fn kind_column(kind: ContributionKind) -> &'static str {
    match kind {
        ContributionKind::Library => "library",
        ContributionKind::Rerender => "rerender",
        ContributionKind::Pattern => "pattern",
        ContributionKind::Gradient => "gradient",
        ContributionKind::Chart => "chart",
    }
}

#[allow(clippy::too_many_arguments)]
async fn insert_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
    kind: ContributionKind,
    intent: ContributionIntent,
    target_look: Option<&str>,
    settings: &str,
    card: Option<&str>,
    render_tag: Option<&RenderTag>,
) -> (ContributionId, Vec<String>) {
    let id = ContributionId::new();
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', ?, NULL, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(kind_column(kind))
    .bind(intent.as_str())
    .bind(target_look)
    .bind(card)
    .bind(BODY)
    .bind(FIRMWARE)
    .bind(SIM)
    .bind(settings)
    .bind(created_at)
    .execute(worker.pool())
    .await
    .expect("contribution");

    let mut images = Vec::new();
    for &index in indices {
        let bytes = blob_for(dataset, index);
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
        let image_id = uuid::Uuid::new_v4().to_string();
        let mut meta = metadata(index);
        meta.render_tag = render_tag.cloned();
        let meta = serde_json::to_string(&meta).expect("metadata");
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
        images.push(image_id);
    }
    (id, images)
}

async fn add_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
) -> (ContributionId, Vec<String>) {
    add_contribution_with(
        worker,
        credential,
        dataset,
        indices,
        created_at,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        "[]",
    )
    .await
}

async fn image_field(worker: &Worker, id: &str, column: &str) -> Option<String> {
    let sql = format!("SELECT {column} FROM contribution_images WHERE id = ?");
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(id)
        .fetch_one(worker.pool())
        .await
        .expect("image field")
}

async fn count(worker: &Worker, sql: &str) -> i64 {
    let value = sqlx::query_scalar(sql)
        .fetch_one(worker.pool())
        .await
        .expect("count");
    if std::env::var_os("LOOKLAB_DUMP_STATE").is_some() {
        dump_state(worker, sql).await;
    }
    value
}

#[derive(serde::Deserialize)]
struct PersistedArtifact {
    name: String,
    sha256: String,
}

/// Prints the persisted state a missing release can hinge on.
async fn dump_state(worker: &Worker, at: &str) {
    let versions: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, status FROM look_versions WHERE look = ? ORDER BY version")
            .bind(LOOK)
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let manifests: Vec<i64> = sqlx::query_scalar(
        "SELECT v.version FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? ORDER BY v.version",
    )
    .bind(LOOK)
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let attempts: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT states, consecutive, updated_at FROM fit_attempts WHERE look = ?")
            .bind(LOOK)
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let holds: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT consecutive, status, diagnostics FROM fit_holds WHERE look = ?")
            .bind(LOOK)
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let gates: Vec<(i64, String)> =
        sqlx::query_as("SELECT version, status FROM gate_failures WHERE look = ? ORDER BY version")
            .bind(LOOK)
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let jobs: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT kind, status, attempts, run_after FROM jobs ORDER BY created_at, id",
    )
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let publication: Option<(i64, i64, i64)> =
        sqlx::query_as("SELECT generation, published, attempted FROM publication WHERE id = 1")
            .fetch_optional(worker.pool())
            .await
            .unwrap_or_default();
    let applied: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM release_reputation_applied WHERE look = ?")
            .bind(LOOK)
            .fetch_one(worker.pool())
            .await
            .unwrap_or_default();
    let scores: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM recipe_scores")
        .fetch_one(worker.pool())
        .await
        .unwrap_or_default();
    let params: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT version, body, stage FROM release_body_params WHERE look = ? ORDER BY version",
    )
    .bind(LOOK)
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    let pool_state: Vec<(String, String)> =
        sqlx::query_as("SELECT body, fingerprint FROM look_pool_state WHERE look = ?")
            .bind(LOOK)
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    let statuses: Vec<(String, i64)> =
        sqlx::query_as("SELECT status, COUNT(*) FROM contribution_images GROUP BY status")
            .fetch_all(worker.pool())
            .await
            .unwrap_or_default();
    eprintln!("--- worker state at {at}");
    eprintln!("look_versions {versions:?}");
    eprintln!("manifests {manifests:?}");
    for (states, consecutive, updated_at) in &attempts {
        let parsed: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_str(states).unwrap_or_default();
        for (body, state) in parsed {
            let state = match state {
                serde_json::Value::String(text) => {
                    serde_json::from_str(&text).unwrap_or(serde_json::Value::Null)
                }
                other => other,
            };
            eprintln!(
                "main fit {body}: stage {:?}, sweep {:?}, converged {:?} (attempt {consecutive}, updated {updated_at})",
                state.get("stage").and_then(serde_json::Value::as_u64),
                state.get("sweep").and_then(serde_json::Value::as_u64),
                state.get("converged").and_then(serde_json::Value::as_bool),
            );
        }
    }
    if attempts.is_empty() {
        eprintln!("main fit: no pending attempt (the last native fit converged, or none ran)");
    }
    eprintln!("fit_holds {holds:?}");
    eprintln!("gate_failures {gates:?}");
    eprintln!("jobs {jobs:?}");
    eprintln!("publication {publication:?}");
    let release_rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT v.version, m.artifacts FROM look_versions v
         JOIN version_manifests m ON m.look_version_id = v.id
         WHERE v.look = ? ORDER BY v.version",
    )
    .bind(LOOK)
    .fetch_all(worker.pool())
    .await
    .unwrap_or_default();
    for (version, artifacts) in release_rows {
        let Ok(refs) = serde_json::from_str::<Vec<PersistedArtifact>>(&artifacts) else {
            continue;
        };
        let Some(artifact) = refs
            .iter()
            .find(|artifact| artifact.name == looklab_worker::refit::RELEASE_ARTIFACT)
        else {
            continue;
        };
        let Ok(bytes) = std::fs::read(worker.config().blobs.join(&artifact.sha256)) else {
            continue;
        };
        let Ok(release) = serde_json::from_slice::<looklab_worker::ReleaseRecord>(&bytes) else {
            continue;
        };
        eprintln!(
            "reference {version}: converged {}, gate passed {}, anchor {}, gauge {:.3}",
            release.converged, release.gate.passed, release.anchor, release.gauge
        );
    }
    eprintln!("reputation rows {applied}; recipe scores {scores}; body params {params:?}");
    eprintln!("pool state {pool_state:?}; image statuses {statuses:?}");
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn duplicates_are_merged_and_a_mislabelled_upload_is_quarantined() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let config = SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    };
    let truth = looklab_fit::synth::truth_look(config.seed);
    let other = mislabelled_look(&truth);
    let dataset = looklab_fit::synth::dataset_with(
        &config,
        &truth,
        &LookSelector::Replace {
            image: 3,
            look: &other,
        },
    );
    let plain = looklab_fit::synth::dataset(&config);
    assert_ne!(
        plain.images[3].samples.out_rgb(),
        dataset.images[3].samples.out_rgb(),
        "the selector did not perturb image 3"
    );
    let credential = add_credential(worker, "new", 0.5).await;
    let (_, images) = add_contribution(
        worker,
        &credential,
        &dataset,
        &[0, 1, 2, 3, 4, 5, 6, 7],
        100,
    )
    .await;
    let (duplicate, duplicate_images) =
        add_contribution(worker, &credential, &dataset, &[4], 200).await;
    let other = add_credential(worker, "new", 0.5).await;
    let (shared, shared_images) = add_contribution(worker, &other, &dataset, &[4], 300).await;
    for id in [&duplicate, &shared] {
        sqlx::query("UPDATE contributions SET serial_hmac = ? WHERE id = ?")
            .bind([7_u8; 32].to_vec())
            .bind(id.as_str())
            .execute(worker.pool())
            .await
            .expect("serial link");
    }

    worker
        .enqueue(&JobPayload::HonestDuplicateMerge, 0)
        .await
        .expect("enqueue merge");
    worker.run_until_idle().await.expect("merge job");
    assert_eq!(
        image_field(worker, &duplicate_images[0], "duplicate_of").await,
        Some(images[4].clone())
    );
    assert_eq!(
        image_field(worker, &shared_images[0], "duplicate_of").await,
        Some(images[4].clone())
    );

    worker
        .enqueue(
            &JobPayload::ConsistencyScore {
                contribution_id: duplicate.clone(),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("jobs");

    assert_eq!(
        image_field(worker, &duplicate_images[0], "duplicate_of").await,
        Some(images[4].clone())
    );
    assert_eq!(
        image_field(worker, &shared_images[0], "duplicate_of").await,
        Some(images[4].clone())
    );
    assert_eq!(
        image_field(worker, &images[3], "status").await.as_deref(),
        Some("quarantined")
    );
    assert_eq!(
        image_field(worker, &images[0], "status").await.as_deref(),
        Some("accepted")
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn nightly_refit_publishes_a_manifest_and_a_verifiable_site() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(
        worker,
        &credential,
        &dataset,
        &[0, 1, 2, 3, 4, 5, 6, 7],
        100,
    )
    .await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("jobs");

    let versions = count(
        worker,
        "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome' AND status = 'published'",
    )
    .await;
    assert_eq!(versions, 1);
    let manifests = count(
        worker,
        "SELECT COUNT(*) FROM version_manifests m JOIN look_versions v ON v.id = m.look_version_id
     WHERE v.look = 'classic-chrome' AND v.version = 1",
    )
    .await;
    assert_eq!(manifests, 1);
    let releases = count(
        worker,
        "SELECT COUNT(*) FROM audit_log WHERE type = 'release_manifest'",
    )
    .await;
    assert_eq!(releases, 1);

    let index_bytes = std::fs::read(fixture.site.join("data/index.json")).expect("index");
    let root = std::fs::read_to_string(fixture.site.join("site-root.txt")).expect("site root");
    let digest = hex(&Sha256::digest(&index_bytes));
    assert_eq!(root.trim(), digest);
    let index: SiteIndex = serde_json::from_slice(&index_bytes).expect("site index");
    let look = index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == LOOK)
        .expect("look indexed");
    assert_eq!(look.versions.len(), 1);
    assert_eq!(look.versions[0].version.as_str(), "1.0.0");

    let manifest_bytes = std::fs::read(
        fixture
            .site
            .join("data/looks/classic-chrome/1.0.0/manifest.json"),
    )
    .expect("manifest");
    let manifest: VersionManifest = serde_json::from_slice(&manifest_bytes).expect("manifest");
    let converged = manifest
        .gate_summary
        .checks
        .iter()
        .find(|check| check.name == "converged")
        .expect("converged check");
    assert_eq!(converged.passed, read_release(&fixture, 1).converged);
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.name.as_str() == "look.looklab")
        .expect("look artifact");
    let bytes = std::fs::read(
        fixture
            .site
            .join("data/looks/classic-chrome/1.0.0/look.looklab"),
    )
    .expect("look bytes");
    verify_artifact(
        &manifest,
        &ArtifactName::new("look.looklab").expect("name"),
        &bytes,
    )
    .expect("artifact hash");
    assert_eq!(artifact.bytes, bytes.len() as u64);
}

#[cfg(feature = "integration")]
#[tokio::test]
async fn a_revert_republishes_the_target_and_the_refit_drops_the_excluded_credential() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    });
    let honest = add_credential(worker, "seeder", 1.2).await;
    let attacker = add_credential(worker, "new", 0.5).await;
    add_contribution(worker, &honest, &dataset, &[0, 1, 2, 3, 4, 5, 6, 7], 100).await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("initial refit");
    add_contribution(worker, &attacker, &dataset, &[4, 5, 6, 7], 200).await;

    let body = looklab_upload::BodyId::new(BODY).expect("body");
    let look = looklab_upload::LookSlug::try_from(LOOK.to_owned()).expect("look");
    let before = looklab_worker::pool::build(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("pool");
    assert!(
        before
            .images
            .iter()
            .any(|image| image.credential.id == attacker)
    );

    let reverted = looklab_server::admin::revert(
        worker.pool(),
        &worker.config().blobs,
        looklab_trust::Actor::Cli,
        &look,
        1,
        std::slice::from_ref(&attacker),
        "attacker contribution",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.new_version, 2);

    let target: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = 1",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("target manifest");
    let republished: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = 2",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("republished manifest");
    let target_artifacts: Vec<PersistedArtifact> =
        serde_json::from_str(&target).expect("target artifacts");
    let republished_artifacts: Vec<PersistedArtifact> =
        serde_json::from_str(&republished).expect("republished artifacts");
    assert_eq!(
        target_artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>(),
        republished_artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>()
    );
    assert_ne!(republished, target);
    let v2_look = republished_artifacts
        .iter()
        .find(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        .expect("look artifact");
    let document = LookDocument::decode(
        &std::fs::read(worker.config().blobs.join(&v2_look.sha256)).expect("look bytes"),
    )
    .expect("document");
    assert_eq!(document.file.look.version.as_str(), "2.0.0");
    let v2_card = republished_artifacts
        .iter()
        .find(|artifact| artifact.name == CARD_ARTIFACT)
        .expect("card artifact");
    let card =
        read_card(&std::fs::read(worker.config().blobs.join(&v2_card.sha256)).expect("card bytes"))
            .expect("card");
    assert_eq!(card.meta().version.as_str(), "2.0.0");
    let v2_release = republished_artifacts
        .iter()
        .find(|artifact| artifact.name == "release.json")
        .expect("release artifact");
    let v2_release: serde_json::Value = serde_json::from_slice(
        &std::fs::read(worker.config().blobs.join(&v2_release.sha256)).expect("release bytes"),
    )
    .expect("release json");
    let v1_release = target_artifacts
        .iter()
        .find(|artifact| artifact.name == "release.json")
        .expect("target release");
    let v1_release: serde_json::Value = serde_json::from_slice(
        &std::fs::read(worker.config().blobs.join(&v1_release.sha256)).expect("target release"),
    )
    .expect("target release json");
    assert_eq!(v2_release["version"], 2);
    assert_eq!(v2_release["anchor"], 1);
    assert_eq!(
        v2_release["gated_since_anchor"],
        v1_release["gated_since_anchor"]
    );

    let after = looklab_worker::pool::build(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("pool");
    assert!(
        after
            .images
            .iter()
            .all(|image| image.credential.id != attacker)
    );
    assert!(
        after
            .images
            .iter()
            .any(|image| image.credential.id == honest)
    );

    worker.run_until_idle().await.expect("revert refit");
    let refit_status: String = sqlx::query_scalar(
        "SELECT status FROM jobs WHERE kind = 'revert_refit' ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(worker.pool())
    .await
    .expect("revert refit job");
    assert_eq!(refit_status, "done");
    let released: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM look_versions
         WHERE look = ? AND version = 3 AND status = 'published'",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("forced release");
    let held: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM gate_failures WHERE look = ? AND version = 3")
            .bind(LOOK)
            .fetch_one(worker.pool())
            .await
            .expect("forced gate");
    assert_eq!(
        released + held,
        1,
        "the forced refit must produce a release or a gate result"
    );
    if released == 1 {
        let refitted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM release_body_params
             WHERE look = ? AND version = 3 AND body = ?",
        )
        .bind(LOOK)
        .bind(BODY)
        .fetch_one(worker.pool())
        .await
        .expect("refit params");
        assert_eq!(refitted, 2);
    }
    let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_refit_requests")
        .fetch_one(worker.pool())
        .await
        .expect("refit request");
    assert_eq!(pending, 0);

    let target_after: String = sqlx::query_scalar(
        "SELECT m.artifacts FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = 1",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("target manifest after refit");
    assert_eq!(target_after, target);
    let exclusions: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM look_credential_exclusions WHERE look = ? AND credential_id = ?",
    )
    .bind(LOOK)
    .bind(attacker.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("exclusion row");
    assert_eq!(exclusions, 1);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn gate_failure_holds_the_candidate_and_opens_a_moderation_item() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let first = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 500,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (first_id, _) =
        add_contribution(worker, &credential, &first, &[0, 1, 2, 3, 4, 5, 6, 7], 100).await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("first release");

    sqlx::query("UPDATE contributions SET status = 'withdrawn' WHERE id = ?")
        .bind(first_id.as_str())
        .execute(worker.pool())
        .await
        .expect("withdraw");

    let truth = looklab_fit::synth::truth_look(21);
    let replacement = looklab_fit::synth::perturbed_look(&truth);
    let second = looklab_fit::synth::dataset_with(
        &SynthConfig {
            seed: 33,
            images: 8,
            samples_per_image: 500,
            ..SynthConfig::default()
        },
        &replacement,
        &LookSelector::Single(&replacement),
    );
    add_contribution(worker, &credential, &second, &[0, 1, 2, 3, 4, 5, 6, 7], 300).await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle()
        .await
        .expect("second release attempt");

    let failures = count(
        worker,
        "SELECT COUNT(*) FROM gate_failures WHERE status = 'pending' AND version = 2",
    )
    .await;
    assert_eq!(failures, 1);
    let published = count(
        worker,
        "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome' AND version = 2",
    )
    .await;
    assert_eq!(published, 0);
}

#[tokio::test]
async fn withdrawal_gc_deletes_blobs_after_retention() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (contribution, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT samples_hash FROM contribution_images WHERE contribution_id = ?",
    )
    .bind(contribution.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));
    assert!(path.exists());
    sqlx::query(
        "INSERT INTO withdrawals (id, contribution_id, credential_id, requested_at, delete_after, status)
         VALUES (?, ?, ?, 0, ?, 'pending')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(contribution.as_str())
    .bind(credential.as_str())
    .bind(0_i64)
    .execute(worker.pool())
    .await
    .expect("withdrawal");

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(contribution.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("gc");

    assert!(!path.exists());
    let blobs = count(worker, "SELECT COUNT(*) FROM blobs").await;
    assert_eq!(blobs, 0);
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM withdrawals WHERE contribution_id = ?")
            .bind(contribution.as_str())
            .fetch_one(worker.pool())
            .await
            .expect("withdrawal status");
    assert_eq!(status.as_deref(), Some("deleted"));
}

#[cfg(feature = "integration")]
async fn admit_root(worker: &Worker, seeder: &CredentialId, contribution: &ContributionId) {
    let scope = serde_json::json!({"rules": [{"body": BODY, "look": LOOK}]}).to_string();
    sqlx::query(
        "INSERT INTO root_admissions
         (id, admitted_by, contribution_id, scope, reason, admitted_at)
         VALUES (?, ?, ?, ?, 'test', 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(seeder.as_str())
    .bind(contribution.as_str())
    .bind(scope)
    .execute(worker.pool())
    .await
    .expect("root admission");
}

async fn withdraw(worker: &Worker, contribution: &ContributionId, credential: &CredentialId) {
    sqlx::query(
        "INSERT INTO withdrawals
         (id, contribution_id, credential_id, requested_at, delete_after, status)
         VALUES (?, ?, ?, 0, 0, 'pending')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(contribution.as_str())
    .bind(credential.as_str())
    .execute(worker.pool())
    .await
    .expect("withdrawal");
}

#[cfg(feature = "integration")]
fn read_release(fixture: &Fixture, version: u32) -> looklab_worker::ReleaseRecord {
    let path = fixture
        .site
        .join(format!("data/looks/{LOOK}/{version}.0.0/release.json"));
    serde_json::from_slice(&std::fs::read(path).expect("release record")).expect("release json")
}

async fn refit(worker: &Worker) {
    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    let drained = worker.run_until_idle().await;
    if std::env::var_os("LOOKLAB_DUMP_STATE").is_some() {
        dump_state(worker, "refit").await;
    }
    drained.expect("refit");
}

#[tokio::test]
async fn a_missing_body_transform_fails_the_job() {
    let fixture = fixture_with_transform(false).await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1], 100).await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    let error = worker.run_once().await.expect_err("missing transform");
    assert!(matches!(error, WorkerError::Data(_)), "{error}");
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_dual_illuminant_transform_file_resolves_per_image() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let transform = "format: looklab-transform/1
version: 2
color_matrices:
  - cct: 2856.0
xyz_to_camera:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
  - cct: 6504.0
xyz_to_camera:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
";
    std::fs::write(
        worker.config().transforms.join("fujifilm-x-t5.yaml"),
        transform,
    )
    .expect("transform");
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    refit(worker).await;
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn root_admissions_feed_the_root_gate_and_the_manifest_dependencies() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 6,
        samples_per_image: 400,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (contribution, _) = add_contribution_with(
        worker,
        &credential,
        &dataset,
        &[0, 1, 2, 3, 4, 5],
        100,
        ContributionKind::Chart,
        ContributionIntent::Native,
        None,
        "[]",
    )
    .await;
    admit_root(worker, &credential, &contribution).await;
    refit(worker).await;

    let release = read_release(&fixture, 1);
    assert_eq!(release.provenance.root_admissions.len(), 1);
    assert!(
        release.gate.root_previous.probes > 0,
        "no root probe measurements: {:?}",
        release.gate.root_previous
    );
    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM version_depends_on_root").await,
        1
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn recipe_data_does_not_change_the_look_and_the_recipe_stage_is_persisted() {
    let plain = fixture().await;
    let recipe = fixture().await;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 6,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    for fixture in [&plain, &recipe] {
        let credential = add_credential(&fixture.worker, "seeder", 1.2).await;
        add_contribution(
            &fixture.worker,
            &credential,
            &dataset,
            &[0, 1, 2, 3, 4, 5],
            100,
        )
        .await;
    }
    let recipe_credential = add_credential(&recipe.worker, "new", 0.5).await;
    add_contribution_with(
        &recipe.worker,
        &recipe_credential,
        &dataset,
        &[0, 1, 2],
        200,
        ContributionKind::Rerender,
        ContributionIntent::Recipe,
        Some(LOOK),
        "[]",
    )
    .await;
    refit(&plain.worker).await;
    refit(&recipe.worker).await;

    let plain_look = std::fs::read(
        plain
            .site
            .join(format!("data/looks/{LOOK}/1.0.0/look.looklab")),
    )
    .expect("plain look");
    let recipe_look = std::fs::read(
        recipe
            .site
            .join(format!("data/looks/{LOOK}/1.0.0/look.looklab")),
    )
    .expect("recipe look");
    let plain_look = looklab_model::LookDocument::decode(&plain_look)
        .expect("plain document")
        .look;
    let recipe_look = looklab_model::LookDocument::decode(&recipe_look)
        .expect("recipe document")
        .look;
    let probes = looklab_model::probes::v1();
    for probe in &probes.probes {
        let without = plain_look.eval(probe.xyz).to_array();
        let with = recipe_look.eval(probe.xyz).to_array();
        for channel in 0..3 {
            assert!(
                (without[channel] - with[channel]).abs() < 1.0e-6,
                "recipe data entered theta_L at {:?}: {without:?} vs {with:?}",
                probe.id
            );
        }
    }

    let manifest_bytes = std::fs::read(
        recipe
            .site
            .join(format!("data/looks/{LOOK}/1.0.0/manifest.json")),
    )
    .expect("manifest");
    let manifest: VersionManifest = serde_json::from_slice(&manifest_bytes).expect("manifest json");
    assert!(
        manifest
            .artifacts
            .iter()
            .any(|artifact| artifact.name.as_str() == "crm.json"),
        "recipe stage control response missing"
    );
    assert_eq!(
        count(
            &recipe.worker,
            "SELECT COUNT(*) FROM release_body_params WHERE version = 1",
        )
        .await,
        2
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn bounties_score_accepted_recipes_and_prune_withdrawn_ones() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 6,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3, 4, 5], 100).await;
    refit(worker).await;

    let recipe_credential = add_credential(worker, "endorsed", 1.0).await;
    let (recipe, _) = add_contribution_with(
        worker,
        &recipe_credential,
        &dataset,
        &[0, 1, 2],
        200,
        ContributionKind::Rerender,
        ContributionIntent::Recipe,
        Some(LOOK),
        "[]",
    )
    .await;
    sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
        .bind(recipe.as_str())
        .execute(worker.pool())
        .await
        .expect("accept recipe");

    worker
        .enqueue(&JobPayload::Bounties, 0)
        .await
        .expect("enqueue bounties");
    worker.run_until_idle().await.expect("bounties");

    let (measured, n_verifications): (f64, i64) = sqlx::query_as(
        "SELECT measured_de, n_verifications FROM recipe_scores
     WHERE target_look = ? AND body_id = ?",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_one(worker.pool())
    .await
    .expect("recipe score");
    assert_eq!(n_verifications, 1);
    assert!(measured.is_finite() && measured >= 0.0, "{measured}");
    assert!(
        count(worker, "SELECT COUNT(*) FROM recipe_observations").await > 0,
        "no recipe observations"
    );

    sqlx::query("UPDATE contributions SET status = 'withdrawn' WHERE id = ?")
        .bind(recipe.as_str())
        .execute(worker.pool())
        .await
        .expect("withdraw recipe");
    worker
        .enqueue(&JobPayload::Bounties, 0)
        .await
        .expect("enqueue bounties");
    worker.run_until_idle().await.expect("bounties");

    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM recipe_observations").await,
        0
    );
    assert_eq!(count(worker, "SELECT COUNT(*) FROM recipe_scores").await, 0);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn card_renders_never_change_a_fit_and_verify_per_firmware() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    refit(worker).await;

    let look_path = fixture
        .site
        .join(format!("data/looks/{LOOK}/1.0.0/look.looklab"));
    let before = std::fs::read(&look_path).expect("published look");
    let card_path = fixture
        .site
        .join(format!("data/looks/{LOOK}/1.0.0/{}", CARD_ARTIFACT));
    let published = read_card(&std::fs::read(&card_path).expect("card artifact")).expect("card");
    let tag = looklab_meta::parse_render_tag(&format!(
        "looklab-card/1 fw=acme-sim/2.0.1 look={LOOK}@1.0.0 cid={}",
        hex(&published.cid_prefix())
    ))
    .expect("render tag");

    let card_credential = add_credential(worker, "endorsed", 1.0).await;
    let (card, card_images) = add_card_contribution(
        worker,
        &card_credential,
        &dataset,
        &[0, 1],
        200,
        &CardDeclaration::Tagged(tag),
    )
    .await;
    // Intake queues the consistency job; for a card batch it validates and queues verification.
    worker
        .enqueue(
            &JobPayload::ConsistencyScore {
                contribution_id: card.clone(),
            },
            0,
        )
        .await
        .expect("enqueue intake");
    refit(worker).await;

    assert_eq!(
        image_field(worker, &card_images[0], "status")
            .await
            .as_deref()
            .and_then(ImageOutcome::parse),
        Some(ImageOutcome::Accepted)
    );
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );
    assert_eq!(std::fs::read(&look_path).expect("look"), before);
    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM release_body_params").await,
        2
    );

    let (firmware, images, median, p95): (String, i64, f64, f64) = sqlx::query_as(
        "SELECT firmware_id, images, median_de, p95_de FROM card_verifications
     WHERE look_slug = ? AND look_version = '1.0.0'",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("card verification row");
    assert_eq!(firmware, "acme-sim");
    assert_eq!(images, 2);
    assert!(median.is_finite() && median >= 0.0, "{median}");
    assert!(p95 >= median && p95.is_finite(), "{p95} < {median}");

    // A tag naming another card is rejected before it can add images to a score.
    let mismatched = looklab_meta::parse_render_tag(&format!(
        "looklab-card/1 fw=acme-sim/2.0.1 look={LOOK}@1.0.0 cid={}",
        hex(&[0_u8; 8])
    ))
    .expect("render tag");
    let (rejected, rejected_images) = add_card_contribution(
        worker,
        &card_credential,
        &dataset,
        &[2],
        300,
        &CardDeclaration::Tagged(mismatched),
    )
    .await;
    worker
        .enqueue(
            &JobPayload::ConsistencyScore {
                contribution_id: rejected.clone(),
            },
            0,
        )
        .await
        .expect("enqueue intake");
    worker.run_until_idle().await.expect("card validation");
    let status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(rejected.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("contribution status");
    assert_eq!(
        ContributionStatus::parse(&status).expect("known status"),
        ContributionStatus::Quarantined
    );
    assert!(
        image_field(worker, &rejected_images[0], "status_reason")
            .await
            .is_some()
    );
    let images: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(images), 0) FROM card_verifications WHERE look_slug = ?",
    )
    .bind(LOOK)
    .fetch_one(worker.pool())
    .await
    .expect("verification images");
    assert_eq!(images, 2);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_release_carries_a_verifiable_preview_lut() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    refit(worker).await;

    let directory = fixture.site.join(format!("data/looks/{LOOK}/1.0.0"));
    let manifest: VersionManifest =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    let bytes = std::fs::read(directory.join("preview.lut33")).expect("preview lut");
    assert_eq!(bytes.len(), looklab_publish::LUT_BYTES);
    verify_artifact(
        &manifest,
        &looklab_publish::ArtifactName::new("preview.lut33").expect("name"),
        &bytes,
    )
    .expect("preview artifact hash");

    let card = std::fs::read(directory.join(looklab_publish::CARD_ARTIFACT)).expect("card");
    verify_artifact(
        &manifest,
        &looklab_publish::ArtifactName::new(looklab_publish::CARD_ARTIFACT).expect("name"),
        &card,
    )
    .expect("card artifact hash");
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn the_site_publishes_scenes_and_every_audit_kind() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    let payload = serde_json::json!({
        "event": "grant_created",
        "actor": credential.as_str(),
        "subject": credential.as_str(),
        "grant": "seeder",
        "reason": "test grant",
    });
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES ('grant_created', ?, 0)")
        .bind(payload.to_string())
        .execute(worker.pool())
        .await
        .expect("audit row");
    refit(worker).await;

    let scenes: looklab_publish::SceneIndex = serde_json::from_slice(
        &std::fs::read(fixture.site.join("data/scenes/index.json")).expect("scene index"),
    )
    .expect("scene index json");
    assert_eq!(scenes.scenes.len(), 3);
    for scene in &scenes.scenes {
        assert!(
            fixture.site.join(scene.path.as_str()).exists(),
            "missing {}",
            scene.path.as_str()
        );
    }

    let log: looklab_publish::AuditLog = serde_json::from_slice(
        &std::fs::read(fixture.site.join("data/audit-log.json")).expect("audit log"),
    )
    .expect("audit log json");
    assert!(
        log.entries
            .iter()
            .any(|entry| matches!(entry.event, looklab_publish::AuditEvent::Grant { .. })),
        "grant not published"
    );
    assert!(
        log.entries
            .iter()
            .any(|entry| matches!(entry.event, looklab_publish::AuditEvent::Release { .. })),
        "release not published"
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn publish_rejects_bytes_that_do_not_match_the_manifest() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    refit(worker).await;

    let directory = fixture.site.join(format!("data/looks/{LOOK}/1.0.0"));
    let manifest: VersionManifest =
        serde_json::from_slice(&std::fs::read(directory.join("manifest.json")).expect("manifest"))
            .expect("manifest json");
    let artifact = manifest
        .artifacts
        .iter()
        .find(|artifact| artifact.name.as_str() == "look.looklab")
        .expect("look artifact");
    std::fs::write(
        worker.config().blobs.join(artifact.sha256.as_str()),
        b"corrupt",
    )
    .expect("corrupt blob");
    worker
        .enqueue(&JobPayload::PublishSite, 0)
        .await
        .expect("enqueue publish");
    let error = worker.run_once().await.expect_err("hash mismatch");
    assert!(matches!(error, WorkerError::Data(_)), "{error}");
}

#[tokio::test]
async fn a_claim_fences_state_changes_by_attempt() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    worker
        .enqueue(&JobPayload::HonestDuplicateMerge, 0)
        .await
        .expect("enqueue");
    let first = looklab_worker::jobs::claim(worker.pool(), 10, 100)
        .await
        .expect("claim")
        .expect("job");
    assert_eq!(first.attempts, 1);
    let second = looklab_worker::jobs::claim(worker.pool(), 10, 200)
        .await
        .expect("reclaim")
        .expect("job");
    assert_eq!(second.id, first.id);
    assert_eq!(second.attempts, 2);
    assert!(
        !looklab_worker::jobs::complete(worker.pool(), &first.id, first.attempts)
            .await
            .expect("complete")
    );
    let status: String = sqlx::query_scalar("SELECT status FROM jobs WHERE id = ?")
        .bind(first.id.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("status");
    assert_eq!(status, "running");
    assert!(
        looklab_worker::jobs::complete(worker.pool(), &second.id, second.attempts)
            .await
            .expect("complete")
    );
}

#[tokio::test]
async fn withdrawal_gc_releases_a_shared_blob_once() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (first, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (second, _) = add_contribution(worker, &credential, &dataset, &[0], 200).await;
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT samples_hash FROM contribution_images WHERE contribution_id = ?",
    )
    .bind(first.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1",).await,
        2
    );
    withdraw(worker, &first, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(first.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("gc");
    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(first.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("retry gc");
    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );

    withdraw(worker, &second, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(second.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker.run_until_idle().await.expect("gc");
    assert!(!path.exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blobs").await, 0);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_consistency_retry_preserves_a_quarantine() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let config = SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 400,
        ..SynthConfig::default()
    };
    let truth = looklab_fit::synth::truth_look(config.seed);
    let other = mislabelled_look(&truth);
    let dataset = looklab_fit::synth::dataset_with(
        &config,
        &truth,
        &LookSelector::Replace {
            image: 3,
            look: &other,
        },
    );
    let credential = add_credential(worker, "new", 0.5).await;
    let (contribution, images) = add_contribution(
        worker,
        &credential,
        &dataset,
        &(0..8).collect::<Vec<_>>(),
        100,
    )
    .await;
    for _ in 0..2 {
        worker
            .enqueue(
                &JobPayload::ConsistencyScore {
                    contribution_id: contribution.clone(),
                },
                0,
            )
            .await
            .expect("enqueue");
        worker.run_until_idle().await.expect("consistency");
        assert_eq!(
            image_field(worker, &images[3], "status").await.as_deref(),
            Some("quarantined")
        );
    }
}

#[tokio::test]
async fn withdrawal_is_terminal_for_automated_status_writes() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (contribution, images) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    sqlx::query("UPDATE contributions SET status = 'withdrawn' WHERE id = ?")
        .bind(contribution.as_str())
        .execute(worker.pool())
        .await
        .expect("withdraw");
    let applied = looklab_worker::db::set_contribution_status(
        worker.pool(),
        &contribution,
        looklab_worker::db::ContributionOutcome::Accepted,
    )
    .await
    .expect("status write");
    assert!(!applied);
    let status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(contribution.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("status");
    assert_eq!(status, "withdrawn");

    let image = looklab_worker::db::ImageId::parse(&images[0]).expect("image id");
    let wrote = looklab_worker::db::write_image_stats(
        worker.pool(),
        &image,
        &looklab_worker::db::ImageStatsWrite {
            rms_de: Some(0.0),
            z: Some(0.0),
            reason: None,
            weight_components: None,
            status: looklab_worker::db::ImageOutcome::Accepted,
            status_reason: None,
        },
    )
    .await
    .expect("stats write");
    assert!(!wrote);
    assert_eq!(
        image_field(worker, &images[0], "status").await.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn an_unchanged_pool_does_not_allocate_another_version() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 4,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    refit(worker).await;
    refit(worker).await;
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn reputation_is_applied_once_per_release() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1], 100).await;
    refit(worker).await;

    let reputation: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert_eq!(
    count(
        worker,
        "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome' AND version = 1",
    )
    .await,
    1
);
    for value in [0.4, 0.9] {
        let mut tx = worker.pool().begin().await.expect("tx");
        let applied =
            looklab_worker::db::apply_reputation_once(&mut tx, LOOK, 1, &credential, value, 0)
                .await
                .expect("replay");
        tx.commit().await.expect("commit");
        assert!(!applied);
    }
    let unchanged: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert!((unchanged - reputation).abs() < 1e-9);
}

#[tokio::test]
async fn non_tolerated_spatial_settings_exclude_every_image() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution_with(
        worker,
        &credential,
        &dataset,
        &[0, 1],
        100,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        r#"[{"key":"clarity","step":2,"source":"reproduced"}]"#,
    )
    .await;
    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    let error = worker.run_once().await.expect_err("excluded images");
    assert!(
        matches!(
            error,
            WorkerError::Fit(looklab_fit::FitError::NoNativeImages)
        ),
        "{error}"
    );
}

#[tokio::test]
async fn pool_conversion_keeps_enum_settings_typed() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution_with(
        worker,
        &credential,
        &dataset,
        &[0],
        100,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        r#"[{"key":"dynamic_range","step":1,"source":"reproduced"},
            {"key":"grain","step":0,"source":"reproduced"},
            {"key":"highlight_tone","step":4,"source":"reproduced"}]"#,
    )
    .await;

    let body = looklab_upload::BodyId::new(BODY).expect("body");
    let look = looklab_upload::LookSlug::new(LOOK).expect("look");
    let pool = looklab_worker::pool::build(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("pool");
    let image = pool.images.first().expect("pooled image");
    let grain = looklab_schema::ControlKey::new("grain").expect("grain");
    assert_eq!(
        image.settings.get(&grain),
        Some(&looklab_model::realization::ControlValue::Enum(
            looklab_schema::EnumValue::new("off").expect("off")
        )),
        "the default grain setting must stay the enum value the tolerance check reads"
    );
    let tone = looklab_schema::ControlKey::new("highlight_tone").expect("highlight tone");
    assert_eq!(
        image.settings.get(&tone),
        Some(&looklab_model::realization::ControlValue::Step(4)),
    );
    assert_eq!(image.exposure_mode.0, 1, "DR200 is exposure mode 1");
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_re_anchor_moves_the_reference_version() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(
        worker,
        &credential,
        &dataset,
        &(0..8).collect::<Vec<_>>(),
        100,
    )
    .await;
    refit(worker).await;

    sqlx::query("UPDATE look_versions SET status = 'rooted' WHERE version = 1")
        .execute(worker.pool())
        .await
        .expect("rooted");
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
     VALUES (?, ?, 2, 'published', NULL, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(LOOK)
    .execute(worker.pool())
    .await
    .expect("version row");
    sqlx::query(
        "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
     SELECT (SELECT id FROM look_versions WHERE look = ? AND version = 2),
            m.artifacts, 0
     FROM version_manifests m
     JOIN look_versions v ON v.id = m.look_version_id
     WHERE v.look = ? AND v.version = 1",
    )
    .bind(LOOK)
    .bind(LOOK)
    .execute(worker.pool())
    .await
    .expect("manifest copy");
    let payload = serde_json::json!({
        "event": "re_anchor",
        "actor": credential.as_str(),
        "look": LOOK,
        "version": 2,
        "reason": "new root session",
    });
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES ('re_anchor', ?, 0)")
        .bind(payload.to_string())
        .execute(worker.pool())
        .await
        .expect("re-anchor");

    add_contribution(
        worker,
        &credential,
        &dataset,
        &(0..8).collect::<Vec<_>>(),
        200,
    )
    .await;
    refit(worker).await;
    let versions = count(
        worker,
        "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
    )
    .await;
    let failures = count(worker, "SELECT COUNT(*) FROM gate_failures").await;
    let report: Option<String> =
        sqlx::query_scalar("SELECT report FROM gate_failures ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(worker.pool())
            .await
            .expect("report");
    let path = fixture
        .site
        .join(format!("data/looks/{LOOK}/3.0.0/release.json"));
    assert!(
        path.exists(),
        "versions {versions}, gate failures {failures}, report {report:?}"
    );
    let release = read_release(&fixture, 3);
    assert_eq!(release.anchor, 2);
}

#[tokio::test]
async fn regional_weights_reach_the_samples_and_down_weighting_halves_the_image() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 3,
        samples_per_image: 400,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (_, images) = add_contribution(worker, &credential, &dataset, &[0, 1, 2], 100).await;
    let body = looklab_upload::BodyId::new(BODY).expect("body");
    let look = looklab_upload::LookSlug::new(LOOK).expect("look");
    let report = looklab_worker::weights::inspect(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("weights");
    assert_eq!(report.images.len(), 3);
    for image in &report.images {
        assert!(
            image.per_region.len() > 1,
            "an image spanning one region cannot show a regional allocation"
        );
        let sum: f64 = image.per_region.values().sum();
        assert!((sum - image.look_weight).abs() < 1.0e-9);
        assert_eq!(image.sample_trust.len(), 400);
        assert_eq!(image.sample_regions.len(), 400);
        let mut counts: std::collections::HashMap<_, usize> = std::collections::HashMap::new();
        for region in image.sample_regions.iter().flatten() {
            *counts.entry(*region).or_insert(0) += 1;
        }
        for (index, region) in image.sample_regions.iter().enumerate() {
            let Some(region) = region else {
                assert_eq!(image.sample_trust[index], 0.0);
                continue;
            };
            let clean = image.per_region.get(region).copied().unwrap_or(0.0)
                * image.sample_trust.len() as f64
                / (image.look_weight * counts.get(region).copied().unwrap_or(0) as f64);
            assert!(
                (f64::from(image.sample_trust[index]) - clean).abs() < 1.0e-6,
                "sample {index} in {region:?}: {} vs {clean}",
                image.sample_trust[index]
            );
        }
        let mean: f64 = image
            .sample_trust
            .iter()
            .map(|value| f64::from(*value))
            .sum::<f64>()
            / 400.0;
        assert!((mean - 1.0).abs() < 1.0e-3, "mean multiplier {mean}");
    }

    let target = report
        .images
        .iter()
        .find(|image| image.image.as_str() == images[1])
        .expect("tracked image");
    let before = target.effective_look_weight;
    sqlx::query("UPDATE contribution_images SET status = 'down_weighted' WHERE id = ?")
        .bind(&images[1])
        .execute(worker.pool())
        .await
        .expect("down-weight");
    let report = looklab_worker::weights::inspect(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("weights");
    let after = report
        .images
        .iter()
        .find(|image| image.image.as_str() == images[1])
        .expect("tracked image")
        .effective_look_weight;
    assert!((after - before * looklab_worker::weights::DOWN_WEIGHT_FACTOR).abs() < 1.0e-12);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_firmware_split_opens_a_worker_proposal_with_evidence() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let config = SynthConfig {
        seed: 21,
        images: 30,
        samples_per_image: 300,
        ..SynthConfig::default()
    };
    let truth = looklab_fit::synth::truth_look(config.seed);
    let shifted = mislabelled_look(&truth);
    let base = looklab_fit::synth::dataset(&config);
    let minority =
        looklab_fit::synth::dataset_with(&config, &shifted, &LookSelector::Single(&shifted));
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (old, _) = add_contribution(
        worker,
        &credential,
        &base,
        &(0..15).collect::<Vec<_>>(),
        100,
    )
    .await;
    let (new, _) = add_contribution(
        worker,
        &credential,
        &minority,
        &(15..30).collect::<Vec<_>>(),
        200,
    )
    .await;
    for (contribution, firmware) in [(&old, "1.50"), (&new, "1.60")] {
        sqlx::query("UPDATE contributions SET firmware_version = ? WHERE id = ?")
            .bind(firmware)
            .bind(contribution.as_str())
            .execute(worker.pool())
            .await
            .expect("firmware");
    }
    refit(worker).await;

    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM proposals WHERE origin = 'worker' AND type = 'epoch_split'",
        )
        .await,
        1
    );
    let (evidence,): (String,) = sqlx::query_as(
        "SELECT evidence FROM proposals WHERE origin = 'worker' AND type = 'epoch_split'",
    )
    .fetch_one(worker.pool())
    .await
    .expect("evidence");
    let evidence: serde_json::Value = serde_json::from_str(&evidence).expect("evidence json");
    assert!(
        evidence["p_value"].as_f64().expect("p") < 0.001,
        "{evidence}"
    );
    assert!(
        evidence["effect_de"].as_f64().expect("effect") >= 1.0,
        "{evidence}"
    );
    assert_eq!(evidence["images_from"].as_u64(), Some(15));
    assert_eq!(evidence["images_to"].as_u64(), Some(15));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM audit_log WHERE type = 'proposal_opened'",
        )
        .await,
        1
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_non_converged_batch_publishes_nothing_and_resumes_to_convergence() {
    let fixture = fixture_with(true, |config| {
        config.sweep_budget = Some(2_000.0);
    })
    .await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1], 100).await;

    refit(worker).await;
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        0
    );
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM fit_attempts WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );

    let mut resumed_config = worker.config().clone();
    resumed_config.sweep_budget = None;
    let resumed = Worker::open(resumed_config).await.expect("worker");
    refit(&resumed).await;
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );
    assert!(read_release(&fixture, 1).converged);
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM fit_attempts WHERE look = 'classic-chrome'",
        )
        .await,
        0
    );
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM fit_holds WHERE status = 'pending'",
        )
        .await,
        0
    );
}

#[tokio::test]
async fn three_non_converged_batches_open_one_moderation_item() {
    let fixture = fixture_with(true, |config| {
        config.sweep_budget = Some(1.0);
        config.max_consecutive_nonconverged = 3;
    })
    .await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1], 100).await;

    for _ in 0..4 {
        refit(worker).await;
    }
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        0
    );
    assert_eq!(count(worker, "SELECT COUNT(*) FROM fit_holds").await, 1);
    let queue = looklab_server::admin::mod_queue(worker.pool(), looklab_server::Tier::Seeder)
        .await
        .expect("moderation queue");
    assert_eq!(queue.fit_holds.len(), 1);
    assert_eq!(queue.fit_holds[0].consecutive, 4);
}

#[tokio::test]
#[ignore = "release-mode timing check for 1000 images"]
#[cfg(feature = "full-scale")]
async fn refit_1000_images_under_target() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1000,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let indices: Vec<usize> = (0..1000).collect();
    add_contribution(worker, &credential, &dataset, &indices, 100).await;

    worker
        .enqueue(
            &JobPayload::NightlyRefit {
                bodies: vec![looklab_upload::BodyId::new(BODY).expect("body")],
            },
            0,
        )
        .await
        .expect("enqueue");
    let started = std::time::Instant::now();
    worker.run_once().await.expect("refit").expect("job");
    let elapsed = started.elapsed();
    println!("1000-image Look refit: {elapsed:?}");
    assert!(elapsed.as_secs() < 900, "{elapsed:?}");
    assert_eq!(
    count(
        worker,
        "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome' AND status = 'published'",
    )
    .await,
    1,
    "one 1k batch published no version in {elapsed:?}",
);
    let release = read_release(&fixture, 1);
    assert!(
        release.converged,
        "the 1k batch did not converge in {elapsed:?}",
    );
}
