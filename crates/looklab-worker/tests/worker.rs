// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};

use looklab_fit::FitImage;
use looklab_fit::synth::{LookSelector, SynthConfig, SyntheticDataset};
use looklab_publish::{ArtifactName, SiteIndex, VersionManifest, verify_artifact};
use looklab_samples::{SampleSet, encode};
use looklab_server::{ContributionId, CredentialId};
use looklab_upload::ManifestMetadata;
use looklab_worker::{JobPayload, Worker, WorkerConfig};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BODY: &str = "fujifilm/x-t5";
const LOOK: &str = "classic-chrome";
const FIRMWARE: &str = "1.50";
const SIM: &str = "classic_chrome";

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
    let mut config = WorkerConfig::new(&db, &blobs);
    config.data = data;
    config.site = site.clone();
    config.transforms = dir.path().join("transforms");
    let worker = Worker::open(config).await.expect("worker");
    Fixture {
        _dir: dir,
        worker,
        site,
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
        look: None,
    }
}

fn blob_for(image: &FitImage) -> Vec<u8> {
    let samples = &image.samples;
    let in_rgb: Vec<[f32; 3]> = samples
        .in_rgb()
        .iter()
        .map(|rgb| {
            let xyz = image.transform.mul_vec(rgb.map(f64::from));
            [xyz[0] as f32, xyz[1] as f32, xyz[2] as f32]
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn add_contribution(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
    created_at: i64,
) -> (ContributionId, Vec<String>) {
    let id = ContributionId::new();
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          target_look_id, body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native', NULL, ?, ?, ?, '{}',
                 'pending', ?)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(BODY)
    .bind(FIRMWARE)
    .bind(SIM)
    .bind(created_at)
    .execute(worker.pool())
    .await
    .expect("contribution");

    let mut images = Vec::new();
    for &index in indices {
        let bytes = blob_for(&dataset.images[index]);
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
        let meta = serde_json::to_string(&metadata(index)).expect("metadata");
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

async fn image_field(worker: &Worker, id: &str, column: &str) -> Option<String> {
    let sql = format!("SELECT {column} FROM contribution_images WHERE id = ?");
    sqlx::query_scalar::<_, Option<String>>(&sql)
        .bind(id)
        .fetch_one(worker.pool())
        .await
        .expect("image field")
}

async fn count(worker: &Worker, sql: &str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(worker.pool())
        .await
        .expect("count")
}

#[tokio::test]
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
    let other = looklab_fit::synth::perturbed_look(&truth);
    let dataset = looklab_fit::synth::dataset_with(
        &config,
        &truth,
        &LookSelector::Replace {
            image: 3,
            look: &other,
        },
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
    let root = std::fs::read_to_string(fixture.site.parent().expect("site").join("site-root.txt"))
        .expect("site root");
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

#[tokio::test]
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

#[tokio::test]
#[ignore = "release-mode timing check for 1000 images"]
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
}
