// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::Path;
#[cfg(feature = "integration")]
use std::path::PathBuf;

#[cfg(feature = "integration")]
use looklab_export::card::read_card;
#[cfg(feature = "integration")]
use looklab_fit::synth::LookSelector;
use looklab_fit::synth::{SynthConfig, SyntheticDataset};
#[cfg(feature = "integration")]
use looklab_model::LookDocument;
#[cfg(feature = "integration")]
use looklab_publish::{ArtifactName, CARD_ARTIFACT, SiteIndex, VersionManifest, verify_artifact};
use looklab_samples::{SampleSet, encode};
#[cfg(feature = "integration")]
use looklab_server::admin::Restoration;
use looklab_server::{ContributionId, CredentialId};
#[cfg(feature = "integration")]
use looklab_upload::CardDeclaration;
use looklab_upload::{BodyId, ContributionKind, LookSlug, ManifestMetadata, RenderTag};
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

/// Jobs one fixture queue may run before it must settle.
const DRAIN_JOBS: usize = 64;
const DRAIN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(900);

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
    config.snapshot_dir = dir.path().join("community");
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
        look: Some(looklab_upload::LookSettings {
            white_balance: Some(looklab_upload::Tagged {
                raw: 0x0002,
                value: Some(looklab_upload::WhiteBalanceMode::Daylight),
            }),
            ..Default::default()
        }),
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
    insert_contribution_for(
        worker,
        credential,
        dataset,
        indices,
        created_at,
        kind,
        intent,
        target_look,
        settings,
        card,
        render_tag,
        BODY,
        SIM,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn insert_contribution_for(
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
    body: &str,
    sim: &str,
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
    .bind(body)
    .bind(FIRMWARE)
    .bind(sim)
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

#[cfg(feature = "integration")]
async fn add_contribution_for_body(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
    body: &str,
    sim: &str,
) -> (ContributionId, Vec<String>) {
    insert_contribution_for(
        worker,
        credential,
        dataset,
        indices,
        created_at,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        "[]",
        None,
        None,
        body,
        sim,
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
            .find(|artifact| artifact.name == looklab_publish::RELEASE_ARTIFACT)
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
            release.converged,
            release.gate.as_ref().is_some_and(|gate| gate.passed),
            release.anchor,
            release.gauge
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("merge job");
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");

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
    add_contribution_for_body(
        worker,
        &credential,
        &dataset,
        &[0, 1],
        100,
        "fujifilm/x-s10",
        "provia",
    )
    .await;

    worker
        .enqueue(&JobPayload::NightlyRefit { bodies: Vec::new() }, 0)
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");

    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'provia'"
        )
        .await,
        0,
        "an uncalibrated body must not publish a fit"
    );

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
    let ready = index
        .bodies
        .iter()
        .find(|body| body.id.as_str() == BODY)
        .expect("calibrated body indexed");
    assert_eq!(
        ready.preparation.readiness,
        looklab_publish::PreparationReadiness::Ready
    );
    let uncalibrated = index
        .bodies
        .iter()
        .find(|body| body.id.as_str() == "fujifilm/x-s10")
        .expect("uncalibrated body catalogued");
    assert_eq!(
        uncalibrated.preparation.readiness,
        looklab_publish::PreparationReadiness::Unavailable
    );
    assert_eq!(
        uncalibrated.preparation.blocker,
        Some(looklab_publish::PreparationBlocker::InputTransformMissing)
    );
    let look = index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == LOOK)
        .expect("look indexed");
    assert_eq!(look.versions.len(), 1);
    assert_eq!(look.versions[0].version.as_str(), "1.0.0");
    assert!(
        look.versions[0].card_eligible,
        "a publishable target with a manifested card is card-eligible without a realization"
    );

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
async fn a_revert_republishes_the_target_and_the_refit_drops_the_rejected_contribution() {
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("initial refit");
    let (attacker_contribution, _) =
        add_contribution(worker, &attacker, &dataset, &[4, 5, 6, 7], 200).await;
    let (attacker_good, _) = add_contribution(worker, &attacker, &dataset, &[0, 1], 210).await;

    let body = looklab_upload::BodyId::new(BODY).expect("body");
    let look = looklab_upload::LookSlug::try_from(LOOK.to_owned()).expect("look");
    let before = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
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
        std::slice::from_ref(&attacker_contribution),
        "attacker contribution",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.new_version, 2);
    assert_eq!(reverted.restoration, Restoration::Current);

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

    let after = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
    .expect("pool");
    assert!(
        after
            .images
            .iter()
            .all(|image| image.contribution.id != attacker_contribution)
    );
    assert!(
        after
            .images
            .iter()
            .any(|image| image.contribution.id == attacker_good)
    );
    assert!(
        after
            .images
            .iter()
            .any(|image| image.credential.id == honest)
    );

    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("revert refit");
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
        "SELECT COUNT(*) FROM contribution_rejections WHERE contribution_id = ?",
    )
    .bind(attacker_contribution.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("rejection row");
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("first release");

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
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("gc");

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

async fn defer(worker: &Worker, contribution: &ContributionId, due_at: i64) {
    sqlx::query(
        "UPDATE contributions SET render_class = 'monochrome', deferred_until = ? WHERE id = ?",
    )
    .bind(due_at)
    .bind(contribution.as_str())
    .execute(worker.pool())
    .await
    .expect("deferred contribution");
    sqlx::query(
        "UPDATE contribution_images SET render_class = 'monochrome' WHERE contribution_id = ?",
    )
    .bind(contribution.as_str())
    .execute(worker.pool())
    .await
    .expect("deferred images");
    sqlx::query(
        "INSERT INTO deferred_expiries (id, contribution_id, due_at, status)
         VALUES (?, ?, ?, 'pending')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(contribution.as_str())
    .bind(due_at)
    .execute(worker.pool())
    .await
    .expect("expiry");
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
    let drained = worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await;
    if std::env::var_os("LOOKLAB_DUMP_STATE").is_some() {
        dump_state(worker, "refit").await;
    }
    drained.expect("refit");
}

#[tokio::test]
async fn a_missing_body_transform_is_reported_without_failing_the_job() {
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
    let ownership = looklab_worker::jobs::JobOwnership {
        id: looklab_worker::JobId::new(),
        attempts: 1,
    };
    let outcomes = looklab_worker::refit::run(
        worker.pool(),
        worker.config(),
        &[looklab_upload::BodyId::new(BODY).expect("body")],
        &ownership,
    )
    .await
    .expect("refit continues without the transform");
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].1.as_str(), LOOK);
    assert!(
        matches!(
            &outcomes[0].2,
            looklab_worker::refit::RefitOutcome::Unavailable(reason)
                if reason.contains("input transform")
        ),
        "{:?}",
        outcomes[0].2
    );
    assert_eq!(count(worker, "SELECT COUNT(*) FROM look_versions").await, 0);
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
    let gate = release.gate.as_ref().expect("first release gate");
    assert_eq!(release.provenance.root_admissions.len(), 1);
    assert!(
        gate.root_previous.probes > 0,
        "no root probe measurements: {:?}",
        gate.root_previous
    );
    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM version_depends_on_root").await,
        1
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_root_revoked_before_the_manifest_write_holds_the_candidate() {
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
    let root: String = sqlx::query_scalar("SELECT id FROM root_admissions")
        .fetch_one(worker.pool())
        .await
        .expect("root admission");
    let trigger = format!(
        "CREATE TRIGGER revoke_before_manifest AFTER INSERT ON release_body_params
         WHEN NEW.look = '{LOOK}' AND NEW.stage = 'recipe'
         BEGIN UPDATE root_admissions SET revoked_at = 1 WHERE id = '{root}'; END"
    );
    sqlx::query(&trigger)
        .execute(worker.pool())
        .await
        .expect("trigger");

    worker
        .enqueue(&JobPayload::NightlyRefit { bodies: Vec::new() }, 0)
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");

    assert!(
        looklab_worker::db::manifest_artifacts(worker.pool(), LOOK, 1)
            .await
            .expect("manifest")
            .is_none(),
        "a revoked root must not back a new release"
    );
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'"
        )
        .await,
        0
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
async fn bounties_score_pending_recipes_and_prune_withdrawn_ones() {
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
    worker
        .enqueue(&JobPayload::Bounties, 0)
        .await
        .expect("enqueue bounties");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("bounties");

    let (
        measured,
        predicted,
        n_verifications,
        look_version,
        input_transform,
        control_response,
        source_realization,
        source_body_state,
    ): (f64, f64, i64, i64, i64, i64, String, i64) = sqlx::query_as(
        "SELECT measured_de, predicted_de, n_verifications, look_version,
                input_transform_version, control_response_version,
                source_realization, source_body_state
         FROM recipe_scores WHERE target_look = ? AND body_id = ?",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_one(worker.pool())
    .await
    .expect("recipe score");
    assert_eq!(n_verifications, 1);
    assert!(measured.is_finite() && measured >= 0.0, "{measured}");
    assert!(
        predicted.is_finite() && predicted >= 0.0,
        "the realized prediction must be measured, not a baseline: {predicted}"
    );
    assert!(
        count(worker, "SELECT COUNT(*) FROM recipe_observations").await > 0,
        "no recipe observations"
    );
    let head: i64 =
        sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
            .bind(LOOK)
            .fetch_one(worker.pool())
            .await
            .expect("family head");
    assert_eq!(
        look_version, head,
        "recipe scores must pin the selected family target"
    );
    assert!(
        input_transform > 0 && control_response > 0,
        "recipe scores must carry the source realization versions"
    );
    assert!(
        !source_realization.is_empty() && source_body_state > 0,
        "recipe scores must pin the exact source realization and body state"
    );
    let bound: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_realization_revisions
         WHERE id = ? AND body_state_version = ?",
    )
    .bind(&source_realization)
    .bind(source_body_state)
    .fetch_one(worker.pool())
    .await
    .expect("source binding");
    assert_eq!(bound, 1, "the source binding must name the stored revision");
    let observations: Vec<(i64, i64, i64, String, i64)> = sqlx::query_as(
        "SELECT DISTINCT look_version, input_transform_version, control_response_version,
                source_realization, source_body_state
         FROM recipe_observations WHERE target_look = ? AND body_id = ?",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_all(worker.pool())
    .await
    .expect("recipe observations");
    assert_eq!(
        observations,
        vec![(
            head,
            input_transform,
            control_response,
            source_realization,
            source_body_state
        )],
        "observations must pin the selected target and source realization"
    );
    let reachability: Option<i64> = sqlx::query_scalar(
        "SELECT reachable FROM recipe_residuals WHERE target_look = ? AND body_id = ? LIMIT 1",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_one(worker.pool())
    .await
    .expect("recipe residual reachability");
    assert_eq!(
        reachability, None,
        "unassessed reachability must not become a claim"
    );
    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM bounties").await,
        0,
        "unassessed regions do not become bounties"
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("bounties");

    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM recipe_observations").await,
        0
    );
    assert_eq!(count(worker, "SELECT COUNT(*) FROM recipe_scores").await, 0);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_published_but_unselected_version_is_not_a_recipe_target() {
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
    let _ = add_contribution_with(
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

    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES ('unselected-v2', ?, 2, 'published', '', 0)",
    )
    .bind(LOOK)
    .execute(worker.pool())
    .await
    .expect("unselected version");
    sqlx::query(
        "INSERT INTO version_manifests (look_version_id, artifacts, created_at)
         SELECT 'unselected-v2', m.artifacts, 0 FROM version_manifests m
         JOIN look_versions v ON v.id = m.look_version_id
         WHERE v.look = ? AND v.version = 1",
    )
    .bind(LOOK)
    .execute(worker.pool())
    .await
    .expect("unselected manifest");

    worker
        .enqueue(&JobPayload::Bounties, 0)
        .await
        .expect("enqueue bounties");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("bounties");

    let head: i64 =
        sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
            .bind(LOOK)
            .fetch_one(worker.pool())
            .await
            .expect("family head");
    assert_eq!(head, 1, "the family head must stay at the selected version");
    let versions: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT look_version FROM recipe_observations
         WHERE target_look = ? AND body_id = ?",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_all(worker.pool())
    .await
    .expect("recipe observation versions");
    assert_eq!(versions, vec![1], "an unselected version is not analyzed");
    let score_versions: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT look_version FROM recipe_scores WHERE target_look = ? AND body_id = ?",
    )
    .bind(LOOK)
    .bind(BODY)
    .fetch_all(worker.pool())
    .await
    .expect("recipe score versions");
    assert_eq!(
        score_versions,
        vec![1],
        "an unselected version is not scored"
    );
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("card validation");
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("gc");
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("retry gc");
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
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("gc");
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
        worker
            .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
            .await
            .expect("consistency");
        assert_eq!(
            image_field(worker, &images[3], "status").await.as_deref(),
            Some("quarantined")
        );
    }
}

#[tokio::test]
async fn deferred_monochrome_stays_out_of_the_pool_and_never_scores() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (colour, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (deferred, deferred_images) =
        add_contribution(worker, &credential, &dataset, &[1], 200).await;
    defer(worker, &deferred, 1_000).await;

    let pool = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &BodyId::new(BODY).expect("body"),
        &LookSlug::try_from(LOOK).expect("look"),
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
    .expect("pool");
    assert_eq!(pool.images.len(), 1);
    assert_eq!(pool.images[0].contribution.id, colour);

    worker
        .enqueue(
            &JobPayload::ConsistencyScore {
                contribution_id: deferred.clone(),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("consistency");

    let status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(deferred.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("status");
    assert_eq!(status, "pending");
    for image in &deferred_images {
        let status: String =
            sqlx::query_scalar("SELECT status FROM contribution_images WHERE id = ?")
                .bind(image)
                .fetch_one(worker.pool())
                .await
                .expect("image status");
        assert_eq!(status, "pending");
    }
    let reputation: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert_eq!(reputation, 0.5);
}

#[tokio::test]
async fn deferred_serial_links_still_merge_colour_identity() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let colour_credential = add_credential(worker, "new", 0.5).await;
    let deferred_credential = add_credential(worker, "new", 0.5).await;
    let (colour, _) = add_contribution(worker, &colour_credential, &dataset, &[0], 100).await;
    let (deferred, _) = add_contribution(worker, &deferred_credential, &dataset, &[1], 200).await;
    defer(worker, &deferred, 1_000).await;
    let hmac = vec![7_u8; 32];
    for contribution in [&colour, &deferred] {
        sqlx::query("UPDATE contributions SET serial_hmac = ? WHERE id = ?")
            .bind(hmac.as_slice())
            .bind(contribution.as_str())
            .execute(worker.pool())
            .await
            .expect("serial link");
    }

    let pool = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &BodyId::new(BODY).expect("body"),
        &LookSlug::try_from(LOOK).expect("look"),
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
    .expect("pool");
    assert_eq!(pool.images.len(), 1);
    assert_eq!(pool.images[0].contribution.id, colour);
    let a = looklab_server::admin::trust_id(&colour_credential).expect("colour credential");
    let b = looklab_server::admin::trust_id(&deferred_credential).expect("deferred credential");
    let expected = looklab_trust::EffectiveContributorId::from(std::cmp::min(a, b));
    assert_eq!(pool.images[0].contributor, expected);
}

fn copy_body_files(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination.join("bodies")).expect("snapshot bodies");
    for name in ["fujifilm-x-t5.body.yaml", "fujifilm-x-t5.yaml"] {
        std::fs::copy(
            source.join("bodies").join(name),
            destination.join("bodies").join(name),
        )
        .expect("snapshot body file");
    }
}

fn write_snapshot_manifest(root: &Path, version: u64) -> String {
    let files = ["fujifilm-x-t5.body.yaml", "fujifilm-x-t5.yaml"]
        .into_iter()
        .map(|name| {
            (
                format!("bodies/{name}"),
                std::fs::read(root.join("bodies").join(name)).expect("snapshot file"),
            )
        })
        .collect::<Vec<_>>();
    let hash = looklab_schema::content_hash(&files);
    let manifest = looklab_schema::SnapshotManifest {
        version,
        content_hash: hash.clone(),
    };
    std::fs::write(
        root.join(looklab_schema::SNAPSHOT_MANIFEST),
        serde_json::to_vec(&manifest).expect("manifest json"),
    )
    .expect("snapshot manifest");
    hash
}

#[tokio::test]
async fn a_pool_pins_the_community_snapshot_it_loaded() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let _ = add_contribution(worker, &credential, &dataset, &[0], 100).await;

    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let v5 = worker.config().snapshot_dir.join("v5");
    copy_body_files(&shipped, &v5);
    let hash = write_snapshot_manifest(&v5, 5);
    sqlx::query(
        "INSERT INTO community_snapshot_activation
         (id, version, content_hash, activated_at)
         VALUES (1, 5, ?, 0)",
    )
    .bind(hash)
    .execute(worker.pool())
    .await
    .expect("activation");
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::try_from(LOOK).expect("look");
    let pool = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
    .expect("pool");
    assert_eq!(pool.images.len(), 1);
    assert_eq!(pool.snapshot_version.get(), 5);
    assert_eq!(
        pool.data
            .load(&body)
            .expect("pinned data")
            .body
            .mapping_status,
        looklab_schema::MappingStatus::Provisional
    );

    // A snapshot activated after the pool was built moves neither the recorded
    // version nor the schemas a release reads.
    let later = worker.config().snapshot_dir.join("v6");
    copy_body_files(&shipped, &later);
    let body_path = later.join("bodies/fujifilm-x-t5.body.yaml");
    let text = std::fs::read_to_string(&body_path).expect("body file");
    std::fs::write(
        &body_path,
        text.replace(
            "mapping_status: provisional",
            "mapping_status: fixture_backed",
        ),
    )
    .expect("updated body file");

    let (active, root) = looklab_worker::body::published_snapshot(worker.pool(), worker.config())
        .await
        .expect("active snapshot");
    assert_eq!(active, 5);
    assert_eq!(
        looklab_worker::body::DataDir::new(root)
            .load(&body)
            .expect("active data")
            .body
            .mapping_status,
        looklab_schema::MappingStatus::Provisional
    );
    assert_eq!(pool.snapshot_version.get(), 5);
    assert_eq!(
        pool.data
            .load(&body)
            .expect("pinned data")
            .body
            .mapping_status,
        looklab_schema::MappingStatus::Provisional
    );
}

#[tokio::test]
async fn deferred_expiry_releases_one_reference_and_keeps_a_shared_blob() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (colour, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (deferred, _) = add_contribution(worker, &credential, &dataset, &[0], 200).await;
    defer(worker, &deferred, 0).await;
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT samples_hash FROM contribution_images WHERE contribution_id = ?",
    )
    .bind(deferred.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));
    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        2
    );

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(deferred.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("expiry");

    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );
    let status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(deferred.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("deferred status");
    assert_eq!(status, "withdrawn");
    let expiry: String =
        sqlx::query_scalar("SELECT status FROM deferred_expiries WHERE contribution_id = ?")
            .bind(deferred.as_str())
            .fetch_one(worker.pool())
            .await
            .expect("expiry status");
    assert_eq!(expiry, "deleted");
    let colour_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(colour.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("colour status");
    assert_eq!(colour_status, "pending");

    withdraw(worker, &colour, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(colour.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("withdrawal");
    assert!(!path.exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blobs").await, 0);
}

#[tokio::test]
async fn blob_gc_keeps_a_live_reservation_and_sweeps_it_after_expiry() {
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
    sqlx::query(
        "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at) VALUES (?, ?, 0, ?)",
    )
    .bind(ContributionId::new().as_str())
    .bind(hash.as_slice())
    .bind(looklab_worker::clock::now_unix() + 600)
    .execute(worker.pool())
    .await
    .expect("reservation");

    withdraw(worker, &contribution, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(contribution.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("gc");

    assert!(path.exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blobs").await, 0);
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blob_staging").await, 1);

    sqlx::query("UPDATE blob_staging SET expires_at = 0")
        .execute(worker.pool())
        .await
        .expect("expire");
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: None,
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("sweep");

    assert!(!path.exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blob_staging").await, 0);
}

#[tokio::test]
async fn blob_gc_sweeps_expired_staging_without_a_withdrawal() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let expired = [7u8; 32];
    let live = [8u8; 32];
    for hash in [&expired, &live] {
        std::fs::write(worker.config().blobs.join(hex(hash)), b"blob").expect("blob file");
    }
    for (hash, expires_at) in [
        (&expired, 0),
        (&live, looklab_worker::clock::now_unix() + 600),
    ] {
        sqlx::query(
            "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at) VALUES (?, ?, 0, ?)",
        )
        .bind(ContributionId::new().as_str())
        .bind(hash.as_slice())
        .bind(expires_at)
        .execute(worker.pool())
        .await
        .expect("reservation");
    }

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: None,
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("sweep");

    assert!(!worker.config().blobs.join(hex(&expired)).exists());
    assert!(worker.config().blobs.join(hex(&live)).exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blob_staging").await, 1);
}

#[tokio::test]
async fn blob_gc_keeps_files_named_by_fixtures_and_version_manifests() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let credential = add_credential(worker, "new", 0.5).await;
    let fixture_hash = [9u8; 32];
    let artifact_hash = [10u8; 32];
    for hash in [&fixture_hash, &artifact_hash] {
        std::fs::write(worker.config().blobs.join(hex(hash)), b"blob").expect("blob file");
        sqlx::query(
            "INSERT INTO blob_staging (attempt_id, hash, staged_at, expires_at) VALUES (?, ?, 0, 0)",
        )
        .bind(ContributionId::new().as_str())
        .bind(hash.as_slice())
        .execute(worker.pool())
        .await
        .expect("reservation");
    }
    sqlx::query(
        "INSERT INTO fixture_uploads (sha256, credential_id, file, size, created_at)
         VALUES (?, ?, 'donated.raf', 1, 0)",
    )
    .bind(fixture_hash.as_slice())
    .bind(credential.as_str())
    .execute(worker.pool())
    .await
    .expect("fixture upload");
    let version_id = uuid::Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, 1, 'published', '', 0)",
    )
    .bind(&version_id)
    .bind(LOOK)
    .execute(worker.pool())
    .await
    .expect("look version");
    sqlx::query(
        "INSERT INTO version_manifests (look_version_id, artifacts, created_at) VALUES (?, ?, 0)",
    )
    .bind(&version_id)
    .bind(format!(
        r#"[{{"name":"look.looklab","sha256":"{}"}}]"#,
        hex(&artifact_hash)
    ))
    .execute(worker.pool())
    .await
    .expect("version manifest");

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: None,
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("sweep");

    assert!(worker.config().blobs.join(hex(&fixture_hash)).exists());
    assert!(worker.config().blobs.join(hex(&artifact_hash)).exists());
    assert_eq!(count(worker, "SELECT COUNT(*) FROM blob_staging").await, 0);
}

#[tokio::test]
async fn blob_gc_removes_interrupted_temporary_writes() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let stale = worker.config().blobs.join(".deadbeef.1.1.tmp");
    let fresh = worker.config().blobs.join(".deadbeef.2.2.tmp");
    std::fs::write(&stale, b"partial").expect("stale temporary");
    std::fs::write(&fresh, b"partial").expect("fresh temporary");
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .expect("open stale")
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7_200))
        .expect("modify stale");

    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: None,
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("sweep");

    assert!(!stale.exists());
    assert!(fresh.exists());
}

#[tokio::test]
async fn a_released_image_is_not_released_again_by_expiry() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (colour, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (deferred, _) = add_contribution(worker, &credential, &dataset, &[0], 200).await;
    defer(worker, &deferred, 0).await;
    sqlx::query("DELETE FROM deferred_expiries WHERE contribution_id = ?")
        .bind(deferred.as_str())
        .execute(worker.pool())
        .await
        .expect("clear expiry");
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT samples_hash FROM contribution_images WHERE contribution_id = ?",
    )
    .bind(deferred.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));

    withdraw(worker, &deferred, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(deferred.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("withdrawal");
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );

    sqlx::query(
        "INSERT INTO deferred_expiries (id, contribution_id, due_at, status)
         VALUES (?, ?, 0, 'pending')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(deferred.as_str())
    .execute(worker.pool())
    .await
    .expect("expiry");
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(deferred.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("expiry retry");

    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );
    let colour_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(colour.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("colour status");
    assert_eq!(colour_status, "pending");
}

#[tokio::test]
async fn withdrawing_a_deferred_contribution_cancels_its_expiry() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (colour, _) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (deferred, _) = add_contribution(worker, &credential, &dataset, &[0], 200).await;
    defer(worker, &deferred, 1_000).await;
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT samples_hash FROM contribution_images WHERE contribution_id = ?",
    )
    .bind(deferred.as_str())
    .fetch_one(worker.pool())
    .await
    .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));

    withdraw(worker, &deferred, &credential).await;
    worker
        .enqueue(
            &JobPayload::BlobGc {
                contribution_id: Some(deferred.clone()),
            },
            0,
        )
        .await
        .expect("enqueue");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("withdrawal");

    assert!(path.exists());
    assert_eq!(
        count(worker, "SELECT ref_count FROM blobs LIMIT 1").await,
        1
    );
    let expiry: String =
        sqlx::query_scalar("SELECT status FROM deferred_expiries WHERE contribution_id = ?")
            .bind(deferred.as_str())
            .fetch_one(worker.pool())
            .await
            .expect("expiry status");
    assert_eq!(expiry, "deleted");
    let colour_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(colour.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("colour status");
    assert_eq!(colour_status, "pending");
}

#[tokio::test]
async fn deferred_recipe_and_card_rows_are_not_scored_or_verified() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 1,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "new", 0.5).await;
    let (recipe, _) = add_contribution_with(
        worker,
        &credential,
        &dataset,
        &[0],
        100,
        ContributionKind::Library,
        ContributionIntent::Recipe,
        Some(LOOK),
        "[]",
    )
    .await;
    defer(worker, &recipe, 1_000).await;
    let (card, _) = insert_contribution(
        worker,
        &credential,
        &dataset,
        &[0],
        200,
        ContributionKind::Rerender,
        ContributionIntent::Card,
        None,
        "[]",
        None,
        None,
    )
    .await;
    defer(worker, &card, 1_000).await;

    worker
        .enqueue(&JobPayload::Bounties, 0)
        .await
        .expect("enqueue bounties");
    worker
        .enqueue(&JobPayload::CardVerification, 0)
        .await
        .expect("enqueue cards");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("jobs");

    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM recipe_observations").await,
        0
    );
    assert_eq!(count(worker, "SELECT COUNT(*) FROM bounties").await, 0);
    assert_eq!(
        count(worker, "SELECT COUNT(*) FROM card_verifications").await,
        0
    );
    for contribution in [&recipe, &card] {
        let status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
            .bind(contribution.as_str())
            .fetch_one(worker.pool())
            .await
            .expect("status");
        assert_eq!(status, "pending");
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
    let pool = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools")
    .pop()
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
    assert_eq!(evidence["target_version"].as_u64(), Some(1));
    assert_eq!(evidence["realization"]["epoch"].as_u64(), Some(1));
    assert_eq!(evidence["realization"]["native_sim"].as_str(), Some(SIM));
    assert!(evidence["sources"]["fitter"].as_str().is_some());
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
async fn one_firmware_version_does_not_open_a_split_proposal() {
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
    for contribution in [&old, &new] {
        sqlx::query("UPDATE contributions SET firmware_version = '1.50' WHERE id = ?")
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
        0
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_quarantined_row_does_not_enter_a_split_proposal() {
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
    let (new, new_images) = add_contribution(
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
    sqlx::query("UPDATE contribution_images SET status = 'quarantined' WHERE id = ?")
        .bind(&new_images[0])
        .execute(worker.pool())
        .await
        .expect("quarantine");
    refit(worker).await;

    let (evidence,): (String,) = sqlx::query_as(
        "SELECT evidence FROM proposals WHERE origin = 'worker' AND type = 'epoch_split'",
    )
    .fetch_one(worker.pool())
    .await
    .expect("evidence");
    let evidence: serde_json::Value = serde_json::from_str(&evidence).expect("evidence json");
    assert_eq!(evidence["images_from"].as_u64(), Some(15));
    assert_eq!(evidence["images_to"].as_u64(), Some(14));
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
    let catalog = looklab_schema::ManualCatalog {
        catalog_version: looklab_schema::MANUAL_CATALOG_VERSION,
        cameras: Vec::new(),
    };
    let queue =
        looklab_server::admin::mod_queue(worker.pool(), looklab_server::Tier::Seeder, &catalog)
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

const REFERENCE_SETTINGS: &str = r#"[{"key":"dynamic_range","step":0,"source":"reproduced"},
    {"key":"highlight_tone","step":0,"source":"reproduced"},
    {"key":"shadow_tone","step":0,"source":"reproduced"},
    {"key":"color","step":0,"source":"reproduced"},
    {"key":"color_chrome_effect","step":0,"source":"reproduced"},
    {"key":"color_chrome_fx_blue","step":0,"source":"reproduced"},
    {"key":"wb_shift_r","step":0,"source":"reproduced"},
    {"key":"wb_shift_b","step":0,"source":"reproduced"}]"#;

const NONDEFAULT_SETTINGS: &str = r#"[{"key":"dynamic_range","step":1,"source":"reproduced"},
    {"key":"highlight_tone","step":0,"source":"reproduced"},
    {"key":"shadow_tone","step":0,"source":"reproduced"},
    {"key":"color","step":0,"source":"reproduced"},
    {"key":"color_chrome_effect","step":0,"source":"reproduced"},
    {"key":"color_chrome_fx_blue","step":0,"source":"reproduced"},
    {"key":"wb_shift_r","step":0,"source":"reproduced"},
    {"key":"wb_shift_b","step":0,"source":"reproduced"}]"#;

fn install_two_epoch_snapshot(worker: &Worker) {
    let shipped = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let root = worker.config().snapshot_dir.join("v1");
    copy_body_files(&shipped, &root);
    let path = root.join("bodies/fujifilm-x-t5.yaml");
    let schema = std::fs::read_to_string(&path).expect("schema");
    let start = schema.find("  - id: 1").expect("epoch one");
    let second = schema[start..].replace("  - id: 1", "  - id: 2").replace(
        "firmware: { from: \"1.00\", to: \"1.99\" }",
        "firmware: { from: \"2.00\", to: \"2.99\" }",
    );
    std::fs::write(&path, format!("{schema}{second}")).expect("two-epoch schema");
}

#[tokio::test]
async fn experimental_settings_cannot_change_qualified_weights() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let qualified = add_credential(worker, "seeder", 1.2).await;
    add_contribution_with(
        worker,
        &qualified,
        &dataset,
        &[0],
        100,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        REFERENCE_SETTINGS,
    )
    .await;
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let mut combination = looklab_schema::SettingCombination::new();
    combination.insert(
        looklab_schema::ControlKey::new("dynamic_range").expect("control"),
        looklab_schema::SettingValue::Enum(looklab_schema::EnumValue::new("dr100").expect("value")),
    );
    for name in [
        "highlight_tone",
        "shadow_tone",
        "color",
        "color_chrome_effect",
        "color_chrome_fx_blue",
        "wb_shift_r",
        "wb_shift_b",
    ] {
        combination.insert(
            looklab_schema::ControlKey::new(name).expect("control"),
            looklab_schema::SettingValue::Step(0),
        );
    }
    let mut mask = looklab_schema::SupportedMask::new();
    for index in 0..288 {
        mask.insert(looklab_schema::CellId::new(format!("cell-{index}")).expect("cell"));
    }
    let scope = looklab_worker::pool::EvidenceScope::Qualified {
        report: looklab_schema::ReportScope {
            points: vec![looklab_schema::AssessedPoint {
                settings: combination,
                firmware: None,
                wb_mode: None,
            }],
            combination_basis: looklab_schema::CombinationBasis::Enumerated,
            defaults_assessed: false,
            output_encoding: looklab_schema::OutputEncoding::Srgb,
            colour_cells: Vec::new(),
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body.clone()],
            assessed_domain: looklab_schema::DomainId::new("supported-probes").expect("domain"),
        },
        mask,
        pinned: looklab_worker::assessment::PinnedModes {
            firmware: vec![looklab_schema::FirmwareId {
                body: body.clone(),
                epoch: looklab_schema::ControlEpochId::new(1),
                version: "1.50".parse().expect("firmware"),
            }],
            wb_modes: vec![looklab_schema::WbModeKey::new("daylight".to_owned()).expect("wb")],
        },
    };
    let pools =
        looklab_worker::pool::build_all(worker.pool(), worker.config(), &body, &look, &scope)
            .await
            .expect("pools");
    let before = looklab_worker::weights::weigh_family(
        &pools,
        &looklab_trust::TrustParams::default(),
        &looklab_worker::weights::RootWeights::default(),
        &looklab_worker::weights::ConsistencyScores::default(),
    )
    .expect("weights");
    let before_weights: std::collections::BTreeMap<_, _> = before
        .images
        .iter()
        .map(|entry| (entry.image, entry.effective_look_weight()))
        .collect();
    assert_eq!(before_weights.len(), 1);

    let experimental = add_credential(worker, "new", 0.5).await;
    let (contribution, _) = add_contribution_with(
        worker,
        &experimental,
        &dataset,
        &[1],
        200,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        NONDEFAULT_SETTINGS,
    )
    .await;

    let pools =
        looklab_worker::pool::build_all(worker.pool(), worker.config(), &body, &look, &scope)
            .await
            .expect("pools");
    let after = looklab_worker::weights::weigh_family(
        &pools,
        &looklab_trust::TrustParams::default(),
        &looklab_worker::weights::RootWeights::default(),
        &looklab_worker::weights::ConsistencyScores::default(),
    )
    .expect("weights");
    for (image, weight) in &before_weights {
        let entry = after.get(image).expect("qualified image remains weighted");
        assert_eq!(entry.effective_look_weight(), *weight);
    }
    let out_of_scope: Vec<_> = pools
        .iter()
        .flat_map(|pool| pool.images.iter())
        .filter(|image| image.contribution.id == contribution)
        .collect();
    assert_eq!(out_of_scope.len(), 1);
    assert_eq!(
        out_of_scope[0].eligibility,
        looklab_worker::pool::EvidenceEligibility::Ineligible(vec![
            looklab_worker::pool::EvidenceBlocker::OutOfScope
        ])
    );

    let experimental_pools = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("experimental pools");
    let experimental_image = experimental_pools
        .iter()
        .flat_map(|pool| pool.images.iter())
        .find(|image| image.contribution.id == contribution)
        .expect("experimental image");
    assert!(
        experimental_image.is_eligible(),
        "the excluded observation must remain available to experimental realizations"
    );
}

#[cfg(feature = "integration")]
async fn family_head_version(worker: &Worker, look: &str) -> Option<i64> {
    sqlx::query_scalar("SELECT head_version FROM model_family_heads WHERE family = ?")
        .bind(look)
        .fetch_optional(worker.pool())
        .await
        .expect("family head")
}

#[cfg(feature = "integration")]
async fn effective_look_weights(
    worker: &Worker,
    look: &str,
) -> std::collections::BTreeMap<String, f64> {
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(look).expect("look");
    let report = looklab_worker::weights::inspect(worker.pool(), worker.config(), &body, &look)
        .await
        .expect("weights");
    report
        .images
        .iter()
        .map(|image| (image.image.as_str().to_owned(), image.effective_look_weight))
        .collect()
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn experimental_only_evidence_leaves_a_component_target_unchanged() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 10,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3], 100).await;
    add_contribution_for_body(
        worker,
        &credential,
        &dataset,
        &[4, 5, 6, 7],
        100,
        BODY,
        "provia",
    )
    .await;
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(1));
    assert_eq!(family_head_version(worker, "provia").await, Some(1));

    // Unchanged rerun: the stored family fingerprints match their comparison inputs.
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(1));
    assert_eq!(family_head_version(worker, "provia").await, Some(1));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        1
    );

    // Control: eligible native evidence still advances the component target.
    add_contribution(worker, &credential, &dataset, &[8], 200).await;
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(2));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        2
    );
    let reputation: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    let applied = count(
        worker,
        "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome'",
    )
    .await;
    let weights = effective_look_weights(worker, LOOK).await;

    // Grain is spatial and only "off" is tolerated, so this observation enters no fit.
    let experimental = add_credential(worker, "new", 0.5).await;
    add_contribution_with(
        worker,
        &experimental,
        &dataset,
        &[9],
        300,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        r#"[{"key":"grain","step":1,"source":"reproduced"}]"#,
    )
    .await;
    refit(worker).await;

    assert_eq!(family_head_version(worker, LOOK).await, Some(2));
    assert_eq!(family_head_version(worker, "provia").await, Some(2));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM look_versions WHERE look = 'classic-chrome'",
        )
        .await,
        2
    );
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome'",
        )
        .await,
        applied
    );
    let unchanged: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert!((unchanged - reputation).abs() < 1e-9);
    let experimental_reputation: f64 =
        sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
            .bind(experimental.as_str())
            .fetch_one(worker.pool())
            .await
            .expect("experimental reputation");
    assert_eq!(experimental_reputation, 0.5);
    assert_eq!(effective_look_weights(worker, LOOK).await, weights);
}

#[cfg(feature = "integration")]
fn qualified_report_provenance() -> looklab_schema::ReportProvenance {
    use looklab_schema::{ProbeSetId, RuleRevision, Sha256Hex};
    let sources = looklab_worker::source_identity::sources();
    looklab_schema::ReportProvenance {
        rule_revision: RuleRevision::new("rule/1").expect("rule"),
        fitter: sources.fitter.clone(),
        evaluator: sources.evaluator.clone(),
        model: sources.model.clone(),
        probe_set: ProbeSetId::new("v1").expect("probes"),
        effective_options: Sha256Hex::from_bytes([0; 32]),
        partition_policy: Sha256Hex::from_bytes([1; 32]),
        assessment_seeds: vec![1],
    }
}

#[cfg(feature = "integration")]
fn qualified_report_metrics(
    capability: looklab_schema::QualificationCapability,
) -> looklab_schema::ReportMetrics {
    use looklab_schema::{MetricKey, QualificationCapability, ReportMetrics};
    let mut metrics = ReportMetrics::new();
    metrics.insert(MetricKey::SupportedCells, 288.0);
    metrics.insert(MetricKey::AssessedCells, 288.0);
    metrics.insert(MetricKey::Coverage, 0.9);
    match capability {
        QualificationCapability::NativePrediction => {
            metrics.insert(MetricKey::MedianDe, 1.0);
        }
        QualificationCapability::ControlsMatching => {
            metrics.insert(MetricKey::MedianDePostClip, 1.0);
        }
        QualificationCapability::CrossBodyAdmission => {
            metrics.insert(MetricKey::CrossBodyMedianDisagreementDe, 1.0);
        }
    }
    metrics
}

/// Admits the fitted realization for the firmware the report point names.
#[cfg(feature = "integration")]
async fn admit_qualified_realization(
    worker: &Worker,
    credential: &CredentialId,
    images: &[String],
) {
    use looklab_schema::{
        ArtifactRole, AssessedPoint, AssessmentOutcome, BodyId as SchemaBodyId, BodyRealizationId,
        BodyRealizationVersionId, CellId, CohortAdmissionRequest, CohortMember, CombinationBasis,
        ControlEpochId, DependencyKind, DependencyRef, DependencyTarget, DomainId,
        EnginePrerequisite, FamilyTargetId, FirmwareId, LookId, MetricKey, ModelReport,
        ModelSubject, ModelVersionId, OutputEncoding, PendingGeneration, ProjectionUse,
        QualificationCapability, RealizationRevisionId, RealizationStatus, ReportMetrics,
        ReportScope, RuleRevision, SettingCombination, SimKey, SourceRef, SubjectKey,
        SupportedMask, TargetVersion,
    };
    use looklab_server::JobId;
    use looklab_server::model_store::{
        self, DiagnosticVerdict, DiagnosticWeightComponents, EvidenceMembership, ExpectedSelection,
        JobLease, QualifiedImageOutcome, QualifiedImageStats, QualifiedImageWrite,
        RealizationSelection, SelectionOutcome,
    };

    let family = LookId::new(LOOK).expect("family");
    let schema_body = SchemaBodyId::new(BODY).expect("body");
    let realization = BodyRealizationId {
        body: schema_body.clone(),
        epoch: ControlEpochId::new(1),
        native_sim: SimKey::new(SIM).expect("sim"),
    };
    let target = FamilyTargetId {
        family: family.clone(),
        version: TargetVersion::new(1).expect("target"),
    };
    let firmware = FirmwareId {
        body: schema_body.clone(),
        epoch: ControlEpochId::new(1),
        version: "1.50".parse().expect("firmware"),
    };
    let domain = DomainId::new("qualified-control").expect("domain");
    let mut cells = Vec::new();
    let mut mask = SupportedMask::new();
    for index in 0..288 {
        let cell = CellId::new(format!("cell-{index}")).expect("cell");
        mask.insert(cell.clone());
        cells.push(cell);
    }
    let scope = ReportScope {
        points: vec![AssessedPoint {
            settings: SettingCombination::new(),
            firmware: Some(firmware),
            wb_mode: None,
        }],
        combination_basis: CombinationBasis::Enumerated,
        defaults_assessed: true,
        output_encoding: OutputEncoding::Srgb,
        colour_cells: cells,
        exposure_cells: Vec::new(),
        assessed_bodies: vec![schema_body.clone()],
        assessed_domain: domain.clone(),
    };
    let other_body = SchemaBodyId::new("sony/a7").expect("other body");
    let mut admission_scope = scope.clone();
    admission_scope.points.push(AssessedPoint {
        settings: SettingCombination::new(),
        firmware: Some(FirmwareId {
            body: other_body.clone(),
            epoch: ControlEpochId::new(1),
            version: "1.00".parse().expect("other firmware"),
        }),
        wb_mode: None,
    });
    admission_scope.assessed_bodies = vec![schema_body.clone(), other_body];
    let subject = ModelSubject::BodyRealization {
        realization: realization.clone(),
    }
    .key();
    let mut conn = worker.pool().acquire().await.expect("connection");
    let stored = model_store::realization_revisions(&mut conn, Some(&subject))
        .await
        .expect("revisions")
        .into_iter()
        .filter(|revision| {
            revision.target == target
                && revision.status == RealizationStatus::Candidate
                && revision
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.role == ArtifactRole::RealizationParams)
        })
        .min_by_key(|revision| revision.version)
        .expect("a candidate revision with realization parameters");
    let version = BodyRealizationVersionId {
        realization: realization.clone(),
        version: stored.version,
        target: target.clone(),
    };
    let revision = RealizationRevisionId::for_version(&version);
    let candidate = ModelVersionId::BodyRealization {
        realization: version.clone(),
    };
    let member_report = model_store::insert_report(
        &mut conn,
        &ModelReport {
            subject: ModelSubject::BodyRealization {
                realization: realization.clone(),
            },
            candidate: candidate.clone(),
            capability: QualificationCapability::NativePrediction,
            scope: scope.clone(),
            provenance: qualified_report_provenance(),
            outcome: AssessmentOutcome::Pass,
            metrics: qualified_report_metrics(QualificationCapability::NativePrediction),
            supported_mask: mask.clone(),
            limitations: Vec::new(),
            evidence_requests: Vec::new(),
        },
        &[],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("member report");
    let mut engine_metrics = ReportMetrics::new();
    engine_metrics.insert(MetricKey::RecoveryRate, 0.9);
    engine_metrics.insert(MetricKey::Coverage, 0.9);
    engine_metrics.insert(MetricKey::AssessedCells, 288.0);
    engine_metrics.insert(MetricKey::SupportedCells, 288.0);
    let engine = model_store::insert_engine_prerequisite(
        &mut conn,
        &EnginePrerequisite {
            engine: SourceRef::new("engine@1").expect("engine"),
            rule_revision: RuleRevision::new("p4.3/1").expect("rule"),
            outcome: AssessmentOutcome::Pass,
            metrics: engine_metrics,
            supported_mask: mask.clone(),
            limitations: Vec::new(),
        },
        0,
    )
    .await
    .expect("engine prerequisite");
    let engine_dependency = DependencyRef::required(
        DependencyKind::EngineQualification,
        DependencyTarget::engine_qualification(&engine),
    );
    let admission = model_store::insert_report(
        &mut conn,
        &ModelReport {
            subject: ModelSubject::SharedFamily {
                family: family.clone(),
            },
            candidate: ModelVersionId::SharedFamily {
                target: target.clone(),
            },
            capability: QualificationCapability::CrossBodyAdmission,
            scope: admission_scope,
            provenance: qualified_report_provenance(),
            outcome: AssessmentOutcome::Pass,
            metrics: qualified_report_metrics(QualificationCapability::CrossBodyAdmission),
            supported_mask: mask.clone(),
            limitations: Vec::new(),
            evidence_requests: Vec::new(),
        },
        &[engine_dependency],
        &[],
        &EvidenceMembership::default(),
        0,
    )
    .await
    .expect("admission report");
    model_store::record_cohort_admission(
        &mut conn,
        &CohortAdmissionRequest {
            family: family.clone(),
            target: target.clone(),
            report: admission,
            members: vec![CohortMember::Admitted {
                realization: version.clone(),
                report: member_report,
                scope: Box::new(scope.clone()),
                supported_mask: mask.clone(),
            }],
            assessed_domain: domain,
        },
        0,
    )
    .await
    .expect("admission");

    let job = JobId::new();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, 'refit', '{}', 'running', 1, 0, 0)",
    )
    .bind(job.as_str())
    .execute(&mut *conn)
    .await
    .expect("running job");
    let lease = JobLease {
        job: &job,
        attempts: 1,
    };
    let mut dependencies =
        model_store::dependencies(&mut conn, "realization_revision", revision.as_str())
            .await
            .expect("revision dependencies");
    dependencies.extend(
        model_store::dependencies(&mut conn, "report", &member_report.to_hex())
            .await
            .expect("report dependencies"),
    );
    let snapshot = model_store::dependency_snapshot(&mut conn, &dependencies)
        .await
        .expect("snapshot");
    let anchor = model_store::subject_state(&mut conn, &SubjectKey::for_family(&family))
        .await
        .expect("subject state")
        .and_then(|state| state.anchor_revision);
    let scope_key = scope.key();
    let sources = looklab_worker::source_identity::sources().storage();
    let outcome = model_store::select_realization(
        &mut conn,
        &RealizationSelection {
            subject: &subject,
            sources: &sources,
            expected_anchor: anchor,
            use_: ProjectionUse::QualifiedMatch,
            capability: QualificationCapability::NativePrediction,
            scope_key: &scope_key,
            revision: &revision,
            target: &target,
            report: Some(&member_report),
            supersedes: &[],
            expected: ExpectedSelection::Absent,
            lease,
            snapshot: &snapshot,
            observed_pending: PendingGeneration::new(0),
        },
        0,
    )
    .await
    .expect("qualified selection");
    assert!(matches!(outcome, SelectionOutcome::Selected), "{outcome:?}");

    let contributor =
        looklab_schema::ContributorRef::new(credential.as_str()).expect("contributor");
    for image in images {
        let stats = QualifiedImageStats {
            image: looklab_schema::ImageRef::new(image.as_str()).expect("image reference"),
            rms_de: 1.0,
            z: 0.0,
            verdict: DiagnosticVerdict::Accepted,
            reason: None,
            weight_components: DiagnosticWeightComponents {
                look: 1.0,
                body: 1.0,
                contributor: contributor.clone(),
            },
        };
        let written = model_store::write_qualified_image_stats(
            &mut conn,
            &QualifiedImageWrite {
                subject: &subject,
                sources: &sources,
                candidate: &candidate,
                scope_key: &scope_key,
                report: &member_report,
                stats: &stats,
                lease,
                snapshot: &snapshot,
            },
            0,
        )
        .await
        .expect("qualified image stats");
        assert!(
            matches!(written, QualifiedImageOutcome::Written),
            "{written:?}"
        );
    }
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn experimental_only_evidence_leaves_a_qualified_target_unchanged() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 8,
        samples_per_image: 300,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (_, qualified_images) =
        add_contribution(worker, &credential, &dataset, &[0, 1, 2, 3, 4, 5], 100).await;
    let (other, _) = add_contribution(worker, &credential, &dataset, &[6], 100).await;
    sqlx::query("UPDATE contributions SET firmware_version = '1.60' WHERE id = ?")
        .bind(other.as_str())
        .execute(worker.pool())
        .await
        .expect("firmware");
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(1));

    admit_qualified_realization(worker, &credential, &qualified_images).await;
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(2));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM release_reputation_applied
             WHERE look = 'classic-chrome' AND version = 2",
        )
        .await,
        1
    );
    let reputation: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert!(
        reputation > 1.2,
        "a qualified release rewards the contributor"
    );
    let applied = count(
        worker,
        "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome'",
    )
    .await;
    let weights = effective_look_weights(worker, LOOK).await;

    // Unchanged rerun: the qualified fingerprint matches its stored value.
    refit(worker).await;
    assert_eq!(family_head_version(worker, LOOK).await, Some(2));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome'",
        )
        .await,
        applied
    );

    // Grain is spatial and only "off" is tolerated, so this observation enters no fit.
    let experimental = add_credential(worker, "new", 0.5).await;
    add_contribution_with(
        worker,
        &experimental,
        &dataset,
        &[7],
        300,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        r#"[{"key":"grain","step":1,"source":"reproduced"}]"#,
    )
    .await;
    refit(worker).await;

    assert_eq!(family_head_version(worker, LOOK).await, Some(2));
    assert_eq!(
        count(
            worker,
            "SELECT COUNT(*) FROM release_reputation_applied WHERE look = 'classic-chrome'",
        )
        .await,
        applied
    );
    let unchanged: f64 = sqlx::query_scalar("SELECT reputation FROM credentials WHERE id = ?")
        .bind(credential.as_str())
        .fetch_one(worker.pool())
        .await
        .expect("reputation");
    assert!((unchanged - reputation).abs() < 1e-9);
    assert_eq!(effective_look_weights(worker, LOOK).await, weights);
}

#[tokio::test]
async fn epochs_of_one_family_stay_distinct_realizations() {
    let fixture = fixture().await;
    install_two_epoch_snapshot(&fixture.worker);
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (second, _) = add_contribution(worker, &credential, &dataset, &[1], 200).await;
    sqlx::query("UPDATE contributions SET firmware_version = '2.50' WHERE id = ?")
        .bind(second.as_str())
        .execute(worker.pool())
        .await
        .expect("firmware");

    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let pools = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools");
    assert_eq!(pools.len(), 2);
    for pool in &pools {
        assert_eq!(pool.realization.body, body);
        assert_eq!(pool.realization.native_sim.as_str(), SIM);
        assert!(
            pool.images
                .iter()
                .all(|image| image.realization == pool.realization)
        );
    }
    let epochs: std::collections::BTreeSet<u32> = pools
        .iter()
        .map(|pool| pool.realization.epoch.get())
        .collect();
    assert_eq!(epochs, std::collections::BTreeSet::from([1, 2]));
    assert_eq!(pools.iter().map(|pool| pool.images.len()).sum::<usize>(), 2);
}

#[tokio::test]
async fn combined_family_weights_cap_across_realizations() {
    let fixture = fixture().await;
    install_two_epoch_snapshot(&fixture.worker);
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 6,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let trusted = add_credential(worker, "seeder", 1.2).await;
    add_contribution(worker, &trusted, &dataset, &[0], 100).await;
    let experimental = add_credential(worker, "new", 0.5).await;
    let (contribution, _) =
        add_contribution(worker, &experimental, &dataset, &[0, 1, 2, 3, 4, 5], 200).await;
    sqlx::query("UPDATE contributions SET firmware_version = '2.50' WHERE id = ?")
        .bind(contribution.as_str())
        .execute(worker.pool())
        .await
        .expect("firmware");

    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let pools = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools");
    assert_eq!(pools.len(), 2);
    let params = looklab_trust::TrustParams::default();
    let root = looklab_worker::weights::RootWeights::default();
    let experimental_ids: std::collections::BTreeSet<_> = pools
        .iter()
        .filter(|pool| pool.realization.epoch.get() == 2)
        .flat_map(|pool| pool.images.iter().map(|image| image.image.id))
        .collect();
    let separate: f64 = pools
        .iter()
        .filter(|pool| pool.realization.epoch.get() == 2)
        .map(|pool| {
            looklab_worker::weights::weigh(
                pool,
                &params,
                &root,
                &looklab_worker::weights::ConsistencyScores::default(),
            )
            .expect("weights")
            .images
            .iter()
            .map(|entry| entry.look_weight)
            .sum::<f64>()
        })
        .sum();
    let combined = looklab_worker::weights::weigh_family(
        &pools,
        &params,
        &root,
        &looklab_worker::weights::ConsistencyScores::default(),
    )
    .expect("combined weights");
    let combined_experimental: f64 = combined
        .images
        .iter()
        .filter(|entry| experimental_ids.contains(&entry.image))
        .map(|entry| entry.look_weight)
        .sum();
    assert!(
        combined_experimental < separate - 1.0e-9,
        "combined {combined_experimental} separate {separate}"
    );
}

#[test]
fn evidence_observation_merge_rejects_conflicting_generations() {
    use looklab_schema::{
        DependencyGeneration, DependencyKind, DependencyRef, DependencyTarget, StandingRef,
    };
    let standing = StandingRef::new("00000000-0000-0000-0000-000000000000").expect("standing");
    let dependency = DependencyRef::required(
        DependencyKind::Standing,
        DependencyTarget::standing(&standing),
    );
    let observation = |generation| looklab_worker::pool::EvidenceObservation {
        snapshot_version: looklab_upload::CommunitySnapshotVersion::new(0),
        dependencies: vec![DependencyGeneration {
            dependency: dependency.clone(),
            generation,
        }],
        root_admissions: Vec::new(),
        native_contributors: std::collections::BTreeMap::new(),
        published_contributors: std::collections::BTreeMap::new(),
    };
    let left = observation(0);
    let right = observation(1);
    assert!(
        looklab_worker::pool::EvidenceObservation::merge([&left, &right]).is_err(),
        "conflicting observations were merged"
    );
}

#[tokio::test]
async fn ineligible_serial_bridge_does_not_merge_admitted_contributors() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 43,
        images: 3,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let a = add_credential(worker, "seeder", 1.2).await;
    let b = add_credential(worker, "seeder", 1.2).await;
    let bridge = add_credential(worker, "seeder", 1.2).await;
    let (ca, _) = add_contribution(worker, &a, &dataset, &[0], 100).await;
    let (cb, _) = add_contribution(worker, &b, &dataset, &[1], 101).await;
    let unknown = r#"[{"key":"bogus","step":0,"source":"reproduced"}]"#;
    let (ce1, _) = add_contribution_with(
        worker,
        &bridge,
        &dataset,
        &[2],
        102,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        unknown,
    )
    .await;
    let (ce2, _) = add_contribution_with(
        worker,
        &bridge,
        &dataset,
        &[2],
        103,
        ContributionKind::Library,
        ContributionIntent::Native,
        None,
        unknown,
    )
    .await;
    let (ce3, _) = add_contribution(worker, &bridge, &dataset, &[2], 104).await;
    for (id, serial) in [(&ca, 1_u8), (&ce1, 1), (&cb, 2), (&ce2, 2), (&ce3, 3)] {
        sqlx::query("UPDATE contributions SET serial_hmac = ? WHERE id = ?")
            .bind([serial; 32].to_vec())
            .bind(id.as_str())
            .execute(worker.pool())
            .await
            .expect("serial");
    }
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let (_, observation, _, _) = observe_worker_pool(worker, &body, &look).await;
    let a = looklab_server::admin::trust_id(&a).expect("trust a");
    let b = looklab_server::admin::trust_id(&b).expect("trust b");
    let bridge = looklab_server::admin::trust_id(&bridge).expect("trust bridge");
    assert_ne!(
        observation.contributor(looklab_worker::pool::EvidenceStage::Native, a),
        observation.contributor(looklab_worker::pool::EvidenceStage::Native, b),
        "ineligible bridge merged native contributors"
    );
    assert_ne!(
        observation.contributor(looklab_worker::pool::EvidenceStage::Native, bridge),
        observation.contributor(looklab_worker::pool::EvidenceStage::Native, a),
        "ineligible serial links on an eligible credential merged native contributors"
    );
    assert_ne!(
        observation.contributor(looklab_worker::pool::EvidenceStage::Published, bridge),
        observation.contributor(looklab_worker::pool::EvidenceStage::Published, a),
        "ineligible bridge joined the published graph"
    );
}

#[tokio::test]
async fn union_contributor_graph_is_shared_across_pools() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 49,
        images: 2,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let a = add_credential(worker, "seeder", 1.2).await;
    let b = add_credential(worker, "seeder", 1.2).await;
    let c = add_credential(worker, "seeder", 1.2).await;
    let (ca, _) = add_contribution(worker, &a, &dataset, &[0], 100).await;
    let (cb1, _) = add_contribution(worker, &b, &dataset, &[1], 101).await;
    let (cb2, _) = add_contribution(worker, &b, &dataset, &[0], 102).await;
    let (cc, _) = add_contribution(worker, &c, &dataset, &[1], 103).await;
    for (id, serial) in [(&ca, 1_u8), (&cb1, 1), (&cb2, 1), (&cc, 2)] {
        sqlx::query("UPDATE contributions SET serial_hmac = ? WHERE id = ?")
            .bind([serial; 32].to_vec())
            .bind(id.as_str())
            .execute(worker.pool())
            .await
            .expect("serial");
    }
    sqlx::query("UPDATE contributions SET firmware_version = '2.50' WHERE id = ?")
        .bind(cb2.as_str())
        .execute(worker.pool())
        .await
        .expect("firmware");
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let all = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        &body,
        &look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools");
    let realizations: std::collections::BTreeSet<_> =
        all.iter().map(|pool| pool.realization.clone()).collect();
    assert!(realizations.len() >= 2, "expected two realizations");
    let mut pools = Vec::new();
    for realization in realizations {
        pools.push(
            looklab_worker::pool::build(
                worker.pool(),
                worker.config(),
                &body,
                &look,
                &realization,
                &looklab_worker::pool::EvidenceScope::Experimental,
            )
            .await
            .expect("pool"),
        );
    }
    looklab_worker::pool::resolve_contributors(&mut pools).expect("resolve");
    let a = looklab_server::admin::trust_id(&a).expect("trust a");
    let b = looklab_server::admin::trust_id(&b).expect("trust b");
    let c = looklab_server::admin::trust_id(&c).expect("trust c");
    for pool in &pools {
        assert_eq!(
            pool.observation
                .contributor(looklab_worker::pool::EvidenceStage::Native, b),
            pool.observation
                .contributor(looklab_worker::pool::EvidenceStage::Native, a),
            "a pool used a different union graph"
        );
        assert_ne!(
            pool.observation
                .contributor(looklab_worker::pool::EvidenceStage::Native, c),
            pool.observation
                .contributor(looklab_worker::pool::EvidenceStage::Native, a),
            "uncoupled control merged"
        );
    }
    let merged = looklab_worker::pool::EvidenceObservation::merge(
        pools.iter().map(|pool| &pool.observation),
    )
    .expect("union graphs must not conflict");
    assert_eq!(
        merged.contributor(looklab_worker::pool::EvidenceStage::Native, b),
        merged.contributor(looklab_worker::pool::EvidenceStage::Native, a)
    );
}

#[tokio::test]
async fn unsupported_output_encoding_does_not_change_qualified_pins() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 47,
        images: 3,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "seeder", 1.2).await;
    let (_, daylight) = add_contribution(worker, &credential, &dataset, &[0], 100).await;
    let (_, tungsten) = add_contribution(worker, &credential, &dataset, &[1], 101).await;
    let image = &tungsten[0];
    let hash: Vec<u8> =
        sqlx::query_scalar("SELECT samples_hash FROM contribution_images WHERE id = ?")
            .bind(image)
            .fetch_one(worker.pool())
            .await
            .expect("hash");
    let path = worker.config().blobs.join(hex(&hash));
    let samples = looklab_samples::decode(&std::fs::read(&path).expect("blob")).expect("decode");
    let mut header = samples.header().clone();
    header.output_space = looklab_samples::OutputSpace::AdobeRgb;
    let set = SampleSet::new(
        header,
        samples.in_rgb().to_vec(),
        samples.out_rgb().to_vec(),
        samples.density_w().to_vec(),
        samples.flags().to_vec(),
    )
    .expect("sample set");
    let bytes = encode(&set).expect("encode");
    let new_hash: [u8; 32] = Sha256::digest(&bytes).into();
    std::fs::write(worker.config().blobs.join(hex(&new_hash)), &bytes).expect("blob");
    sqlx::query("UPDATE contribution_images SET samples_hash = ? WHERE id = ?")
        .bind(new_hash.as_slice())
        .bind(image)
        .execute(worker.pool())
        .await
        .expect("image hash");
    sqlx::query(
        "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
         ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
    )
    .bind(new_hash.as_slice())
    .execute(worker.pool())
    .await
    .expect("blob row");
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let mut mask = looklab_schema::SupportedMask::new();
    for index in 0..288 {
        mask.insert(looklab_schema::CellId::new(format!("cell-{index}")).expect("cell"));
    }
    let scope = looklab_worker::pool::EvidenceScope::Qualified {
        report: looklab_schema::ReportScope {
            points: vec![looklab_schema::AssessedPoint {
                settings: looklab_schema::SettingCombination::new(),
                firmware: None,
                wb_mode: None,
            }],
            combination_basis: looklab_schema::CombinationBasis::Enumerated,
            defaults_assessed: true,
            output_encoding: looklab_schema::OutputEncoding::Srgb,
            colour_cells: Vec::new(),
            exposure_cells: Vec::new(),
            assessed_bodies: vec![body.clone()],
            assessed_domain: looklab_schema::DomainId::new("supported-probes").expect("domain"),
        },
        mask,
        pinned: looklab_worker::assessment::PinnedModes {
            firmware: vec![looklab_schema::FirmwareId {
                body: body.clone(),
                epoch: looklab_schema::ControlEpochId::new(1),
                version: "1.50".parse().expect("firmware"),
            }],
            wb_modes: vec![looklab_schema::WbModeKey::new("daylight".to_owned()).expect("wb")],
        },
    };
    let pools =
        looklab_worker::pool::build_all(worker.pool(), worker.config(), &body, &look, &scope)
            .await
            .expect("pools");
    let images = &pools[0].images;
    let day = images
        .iter()
        .find(|entry| entry.image.id.as_str() == daylight[0].as_str())
        .expect("daylight");
    assert!(day.is_eligible(), "daylight became out of scope");
    let tungsten = images
        .iter()
        .find(|entry| entry.image.id.as_str() == tungsten[0].as_str())
        .expect("tungsten");
    assert!(matches!(
        tungsten.eligibility,
        looklab_worker::pool::EvidenceEligibility::Ineligible(_)
    ));
}

fn observation_body_state(
    body: &BodyId,
    stage: looklab_schema::BodyStateStage,
    version: looklab_schema::BodyStateVersion,
    dependencies: Vec<looklab_schema::DependencyRef>,
) -> looklab_schema::BodyStateRecord {
    use looklab_schema::{ArtifactBinding, ArtifactRole, BodyStatePins, Sha256Hex};
    let binding = |role| ArtifactBinding {
        sha256: Sha256Hex::from_bytes([9; 32]),
        role,
    };
    looklab_schema::BodyStateRecord {
        body: body.clone(),
        version,
        stage,
        pins: BodyStatePins {
            profile: Some(binding(ArtifactRole::CalibrationProfile)),
            input_transform: binding(ArtifactRole::InputTransform),
            body_params: binding(ArtifactRole::BodyRealizationParams),
            gauge: binding(ArtifactRole::Gauge),
        },
        dependencies,
    }
}

async fn observe_worker_pool(
    worker: &Worker,
    body: &BodyId,
    look: &LookSlug,
) -> (
    Vec<looklab_worker::pool::LookPool>,
    looklab_worker::pool::EvidenceObservation,
    Vec<looklab_schema::DependencyGeneration>,
    looklab_schema::BodyRealizationId,
) {
    let pools = looklab_worker::pool::build_all(
        worker.pool(),
        worker.config(),
        body,
        look,
        &looklab_worker::pool::EvidenceScope::Experimental,
    )
    .await
    .expect("pools");
    let realization = pools.first().expect("pool").realization.clone();
    let observation = looklab_worker::pool::EvidenceObservation::merge(
        pools.iter().map(|pool| &pool.observation),
    )
    .expect("merge");
    let dependencies = observation.dependency_refs();
    let snapshot = observation.snapshot_for(&dependencies).expect("snapshot");
    (pools, observation, snapshot, realization)
}

#[tokio::test]
async fn observation_snapshot_fences_metadata_changes_and_survives_append_only_arrivals() {
    use looklab_schema::{
        BodyStateStage, DependencyKind, DependencyRef, DependencyTarget, InvalidationReason,
        ModelSubject,
    };
    use looklab_server::model_store;

    let fixture = fixture().await;
    let worker = &fixture.worker;
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 41,
        images: 3,
        samples_per_image: 200,
        ..SynthConfig::default()
    });
    let credential = add_credential(worker, "endorsed", 1.0).await;
    let (contribution, _) = add_contribution(worker, &credential, &dataset, &[0, 1, 2], 100).await;
    let (recipe, _) = add_contribution_with(
        worker,
        &credential,
        &dataset,
        &[0],
        101,
        ContributionKind::Library,
        ContributionIntent::Recipe,
        Some(LOOK),
        "[]",
    )
    .await;
    let body = BodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let admission = looklab_server::RootAdmissionId::new();
    sqlx::query(
        "INSERT INTO root_admissions
         (id, admitted_by, contribution_id, scope, reason, admitted_at)
         VALUES (?, ?, ?, ?, 'test', 100)",
    )
    .bind(admission.as_str())
    .bind(credential.as_str())
    .bind(contribution.as_str())
    .bind(format!(
        r#"{{"rules":[{{"body":"{BODY}","look":"{LOOK}"}}]}}"#
    ))
    .execute(worker.pool())
    .await
    .expect("root admission");

    let (pools, observation, snapshot, realization) =
        observe_worker_pool(worker, &body, &look).await;
    let native_stage = looklab_worker::pool::stage_dependencies(
        &pools,
        looklab_worker::pool::EvidenceStage::Native,
    );
    let published_stage = looklab_worker::pool::stage_dependencies(
        &pools,
        looklab_worker::pool::EvidenceStage::Published,
    );
    assert!(
        !native_stage.iter().any(|dependency| {
            dependency.kind == DependencyKind::Contribution
                && dependency.target.as_str() == recipe.as_str()
        }),
        "native stage must exclude recipe rows"
    );
    assert!(
        published_stage.iter().any(|dependency| {
            dependency.kind == DependencyKind::Contribution
                && dependency.target.as_str() == recipe.as_str()
        }),
        "published stage must include recipe rows"
    );
    observation
        .snapshot_for(&native_stage)
        .expect("native stage refs were observed");
    observation
        .snapshot_for(&published_stage)
        .expect("published stage refs were observed");
    let dependencies = observation.dependency_refs();
    for kind in [
        DependencyKind::RootAdmission,
        DependencyKind::Standing,
        DependencyKind::IdentityResolution,
        DependencyKind::RootWeighting,
    ] {
        assert!(
            dependencies
                .iter()
                .any(|dependency| dependency.kind == kind),
            "observation has no {kind:?} dependency"
        );
    }
    let subject = ModelSubject::BodyRealization { realization }.key();
    let mut conn = worker.pool().acquire().await.expect("connection");
    let version = model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 0)
        .await
        .expect("reservation");
    model_store::insert_body_state(
        &mut conn,
        &observation_body_state(&body, BodyStateStage::Native, version, dependencies.clone()),
        &snapshot,
        0,
    )
    .await
    .expect("observed evidence commits");

    let pending_before = model_store::pending_generation(&mut conn, &subject)
        .await
        .expect("pending");
    model_store::note_evidence_arrival(&mut conn, &subject, 1)
        .await
        .expect("arrival");
    let pending_after = model_store::pending_generation(&mut conn, &subject)
        .await
        .expect("pending");
    assert!(
        pending_after > pending_before,
        "arrival did not queue a successor"
    );
    let version = model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Published, 1)
        .await
        .expect("reservation");
    model_store::insert_body_state(
        &mut conn,
        &observation_body_state(
            &body,
            BodyStateStage::Published,
            version,
            dependencies.clone(),
        ),
        &snapshot,
        1,
    )
    .await
    .expect("append-only arrival leaves the observation valid");
    let consumed: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM model_pending_consumption WHERE subject_key = ?")
            .bind(subject.as_str())
            .fetch_one(&mut *conn)
            .await
            .expect("consumption");
    assert_eq!(consumed, 0, "successor was consumed by the old snapshot");

    let root_weighting = DependencyRef::required(
        DependencyKind::RootWeighting,
        DependencyTarget::root_weighting_global(),
    );
    model_store::invalidate(
        &mut conn,
        &root_weighting,
        InvalidationReason::RootWeightingChange,
        2,
    )
    .await
    .expect("invalidate root weighting");
    let late = model_store::observe_dependency_snapshot(&mut conn, &dependencies, 2)
        .await
        .expect("late registration");
    let observed = snapshot
        .iter()
        .find(|entry| entry.dependency == root_weighting)
        .expect("observed root weighting")
        .generation;
    let registered = late
        .iter()
        .find(|entry| entry.dependency == root_weighting)
        .expect("late root weighting")
        .generation;
    assert!(
        registered > observed,
        "late registration did not see the change"
    );
    let version = model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 2)
        .await
        .expect("reservation");
    let error = model_store::insert_body_state(
        &mut conn,
        &observation_body_state(&body, BodyStateStage::Native, version, dependencies.clone()),
        &snapshot,
        2,
    )
    .await
    .expect_err("root change must fence the old commit");
    assert!(matches!(
        error,
        model_store::StoreError::StaleDependency { .. }
    ));

    drop(conn);
    let (_pools, observation, snapshot, _) = observe_worker_pool(worker, &body, &look).await;
    let dependencies = observation.dependency_refs();
    let standing = dependencies
        .iter()
        .find(|dependency| dependency.kind == DependencyKind::Standing)
        .cloned()
        .expect("standing dependency");
    sqlx::query("UPDATE credentials SET tier = 'seeder' WHERE id = ?")
        .bind(credential.as_str())
        .execute(worker.pool())
        .await
        .expect("standing");
    let mut conn = worker.pool().acquire().await.expect("connection");
    model_store::invalidate(
        &mut conn,
        &standing,
        InvalidationReason::StandingReinterpretation,
        3,
    )
    .await
    .expect("invalidate standing");
    let version = model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 3)
        .await
        .expect("reservation");
    let error = model_store::insert_body_state(
        &mut conn,
        &observation_body_state(&body, BodyStateStage::Native, version, dependencies.clone()),
        &snapshot,
        3,
    )
    .await
    .expect_err("standing change must fence the old commit");
    assert!(matches!(
        error,
        model_store::StoreError::StaleDependency { .. }
    ));

    drop(conn);
    let (_pools, observation, snapshot, _) = observe_worker_pool(worker, &body, &look).await;
    let dependencies = observation.dependency_refs();
    let identity = dependencies
        .iter()
        .find(|dependency| dependency.kind == DependencyKind::IdentityResolution)
        .cloned()
        .expect("identity dependency");
    sqlx::query("UPDATE contributions SET serial_hmac = ? WHERE id = ?")
        .bind([7_u8; 32].to_vec())
        .bind(contribution.as_str())
        .execute(worker.pool())
        .await
        .expect("identity");
    let mut conn = worker.pool().acquire().await.expect("connection");
    model_store::invalidate(&mut conn, &identity, InvalidationReason::IdentityMerge, 4)
        .await
        .expect("invalidate identity");
    let version = model_store::reserve_body_state(&mut conn, &body, BodyStateStage::Native, 4)
        .await
        .expect("reservation");
    let error = model_store::insert_body_state(
        &mut conn,
        &observation_body_state(&body, BodyStateStage::Native, version, dependencies),
        &snapshot,
        4,
    )
    .await
    .expect_err("identity change must fence the old commit");
    assert!(matches!(
        error,
        model_store::StoreError::StaleDependency { .. }
    ));
}
