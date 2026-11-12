// SPDX-License-Identifier: AGPL-3.0-or-later
//! Manual confirmation state never reaches fit, match or export inputs.

use std::path::Path;

use looklab_export::card::{CardMeta, CardOptions, write_card};
use looklab_fit::synth::{SynthConfig, SyntheticDataset};
use looklab_fit::{FitOptions, fit};
use looklab_match::synth;
use looklab_match::{Emphasis, MatchOptions, PreparedTarget, TargetSpec, optimize};
use looklab_model::LookDocument;
use looklab_model::file::identity;
use looklab_samples::{SampleSet, encode};
use looklab_schema::{
    BodyId, CapabilityKey, ConfirmationScope, ManualCamera, validate_manual_catalog,
};
use looklab_server::{ContributionId, CredentialId};
use looklab_upload::{BodyId as UploadBodyId, LookSlug, ManifestMetadata};
use looklab_worker::{Worker, WorkerConfig};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const BODY: &str = "fujifilm/x-t5";
const LOOK: &str = "classic-chrome";
const FIRMWARE: &str = "1.50";
const SIM: &str = "classic_chrome";
const CONTROL: &str = "native_simulations";

struct Fixture {
    _dir: TempDir,
    worker: Worker,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let db = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let transforms = dir.path().join("transforms");
    std::fs::create_dir_all(&transforms).expect("transform dir");
    std::fs::write(transforms.join("fujifilm-x-t5.yaml"), TRANSFORM).expect("transform file");
    let mut config = WorkerConfig::new(&db, &blobs);
    config.data = data;
    config.snapshot_dir = dir.path().join("community");
    config.site = dir.path().join("site/data");
    config.transforms = transforms;
    let worker = Worker::open(config).await.expect("worker");
    Fixture { _dir: dir, worker }
}

const TRANSFORM: &str = "format: looklab-transform/1
version: 1
camera_to_xyz:
  - [1.0, 0.0, 0.0]
  - [0.0, 1.0, 0.0]
  - [0.0, 0.0, 1.0]
";

async fn add_credential(worker: &Worker) -> CredentialId {
    let id = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', 'seeder', '[]', 1.2, 0, 'active')",
    )
    .bind(id.as_str())
    .bind(id.as_str().as_bytes())
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
        look: Some(looklab_upload::LookSettings::default()),
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

async fn add_images(
    worker: &Worker,
    credential: &CredentialId,
    dataset: &SyntheticDataset,
    indices: &[usize],
) -> ContributionId {
    let id = ContributionId::new();
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          target_look_id, card, body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'library', NULL, 'native',
                 NULL, NULL, ?, ?, ?, '[]', 'pending', 0)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(BODY)
    .bind(FIRMWARE)
    .bind(SIM)
    .execute(worker.pool())
    .await
    .expect("contribution");
    for &index in indices {
        let bytes = blob_for(dataset, index);
        let hash: [u8; 32] = Sha256::digest(&bytes).into();
        std::fs::write(worker.config().blobs.join(hex(&hash)), &bytes).expect("blob file");
        sqlx::query(
            "INSERT INTO blobs (hash, ref_count) VALUES (?, 1)
             ON CONFLICT(hash) DO UPDATE SET ref_count = ref_count + 1",
        )
        .bind(hash.as_slice())
        .execute(worker.pool())
        .await
        .expect("blob row");
        let meta = serde_json::to_string(&metadata(index)).expect("metadata");
        sqlx::query(
            "INSERT INTO contribution_images
             (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status)
             VALUES (?, ?, ?, ?, ?, NULL, 'pending')",
        )
        .bind(uuid::Uuid::new_v4().to_string())
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

async fn add_serial_marker(worker: &Worker, credential: &CredentialId, hmac: [u8; 32]) {
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent,
          body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'chart', ?, 'native',
                 ?, ?, ?, '[]', 'pending', 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(credential.as_str())
    .bind(hmac.to_vec())
    .bind(BODY)
    .bind(FIRMWARE)
    .bind(SIM)
    .execute(worker.pool())
    .await
    .expect("serial marker");
}

fn camera() -> ManualCamera {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/manuals/fujifilm-x-t5.yaml");
    let source = std::fs::read_to_string(path).expect("manual catalog");
    let catalog = validate_manual_catalog(&source).expect("manual catalog");
    catalog
        .camera(&BodyId::new(BODY).expect("body"))
        .cloned()
        .expect("documented camera")
}

fn definition_digest(camera: &ManualCamera) -> looklab_schema::Sha256Hex {
    camera
        .capabilities
        .iter()
        .find(|capability| capability.key == CapabilityKey::NativeSimulations)
        .expect("control")
        .definition_digest(camera)
}

async fn insert_manual_check(
    worker: &Worker,
    credential: &CredentialId,
    camera: &ManualCamera,
    digest: looklab_schema::Sha256Hex,
    ordinal: u8,
) {
    let scope = serde_json::to_string(&ConfirmationScope::Controls {
        keys: vec![CapabilityKey::NativeSimulations],
    })
    .expect("scope");
    let id = sqlx::query(
        "INSERT INTO manual_confirmations
         (credential_id, camera_id, camera_digest, firmware, scope_kind, scope_json,
          payload_digest, assertion_hash, send_nonce, created_at)
         VALUES (?, ?, ?, '2.10', 'controls', ?, ?, x'00', ?, ?)",
    )
    .bind(credential.as_str())
    .bind(camera.id.as_str())
    .bind(camera.definition_digest().as_bytes().as_slice())
    .bind(scope)
    .bind(vec![ordinal; 32])
    .bind(format!("nonce-{ordinal}"))
    .bind(i64::from(ordinal))
    .execute(worker.pool())
    .await
    .expect("confirmation")
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO manual_observations
         (confirmation_id, capability_key, definition_digest, finding_json)
         VALUES (?, ?, ?, '{\"kind\":\"matches\"}')",
    )
    .bind(id)
    .bind(CONTROL)
    .bind(digest.as_bytes().as_slice())
    .execute(worker.pool())
    .await
    .expect("observation");
    sqlx::query(
        "INSERT INTO manual_confirmation_occurrences (confirmation_id, send_nonce, received_at)
         VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(format!("occurrence-{ordinal}"))
    .bind(i64::from(ordinal))
    .execute(worker.pool())
    .await
    .expect("occurrence");
}

async fn clear_manual_checks(worker: &Worker, camera: &ManualCamera) {
    sqlx::query(
        "DELETE FROM manual_confirmation_occurrences
         WHERE confirmation_id IN (SELECT id FROM manual_confirmations WHERE camera_id = ?)",
    )
    .bind(camera.id.as_str())
    .execute(worker.pool())
    .await
    .expect("occurrences");
    sqlx::query(
        "DELETE FROM manual_observations
         WHERE confirmation_id IN (SELECT id FROM manual_confirmations WHERE camera_id = ?)",
    )
    .bind(camera.id.as_str())
    .execute(worker.pool())
    .await
    .expect("observations");
    sqlx::query("DELETE FROM manual_confirmations WHERE camera_id = ?")
        .bind(camera.id.as_str())
        .execute(worker.pool())
        .await
        .expect("confirmations");
}

fn fit_bytes(pools: &[looklab_worker::pool::LookPool]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let scores = looklab_worker::weights::ConsistencyScores::default();
    let pool = pools.first().expect("pool");
    let weighted = looklab_worker::weights::weigh(
        pool,
        &looklab_trust::TrustParams::default(),
        &looklab_worker::weights::RootWeights::default(),
        &scores,
    )
    .expect("weights");
    let contributors = looklab_worker::weights::contributor_index(pool, &scores);
    let images = looklab_worker::weights::fit_images(pool, &weighted, &contributors);
    assert!(!images.is_empty(), "the pool must feed the fit");
    let fitted = fit(
        &images,
        &FitOptions {
            seed: 3,
            max_sweeps: 2,
            sweep_budget: 20_000.0,
            ..FitOptions::default()
        },
    )
    .expect("fit");
    let document = LookDocument::from_look(
        identity(LOOK, "1.0.0").expect("identity"),
        fitted.look.clone(),
    )
    .expect("document");
    let card = write_card(
        &document,
        &CardMeta::new(LOOK, "classic-chrome"),
        &CardOptions::default(),
    )
    .expect("card");
    let fit = format!("{:?}", fitted.look).into_bytes();
    let pool_bytes = format!(
        "{:?}",
        pools
            .iter()
            .map(|pool| pool
                .images
                .iter()
                .map(|image| (&image.image.id, image.is_eligible()))
                .collect::<Vec<_>>())
            .collect::<Vec<_>>()
    )
    .into_bytes();
    (fit, pool_bytes, card)
}

fn match_bytes() -> Vec<u8> {
    let plan = synth::plan();
    let probes = synth::probes();
    let body = synth::body(1).expect("synthetic body");
    let target_function = body.target();
    let target = PreparedTarget::new(TargetSpec {
        probes: &probes,
        function: &target_function,
        uncertainty: None,
        emphasis: Emphasis::Balanced,
        tau: 1.0,
        support_sigma: 1.5,
        density_floor: None,
        appearance: body.appearance(),
    })
    .expect("target");
    let outcome = optimize(
        std::slice::from_ref(&body.model),
        &target,
        &plan,
        &MatchOptions {
            seed: 7,
            ..MatchOptions::default()
        },
    )
    .expect("match");
    serde_json::to_vec(
        &outcome
            .results
            .iter()
            .map(|result| {
                serde_json::json!({
                    "sim": result.sim,
                    "objective": result.objective,
                    "settings": result.settings,
                })
            })
            .collect::<Vec<_>>(),
    )
    .expect("match json")
}

#[tokio::test]
async fn confirmation_state_never_reaches_fit_match_or_export() {
    let fixture = fixture().await;
    let worker = &fixture.worker;
    let camera = camera();
    let digest = definition_digest(&camera);
    let dataset = looklab_fit::synth::dataset(&SynthConfig {
        seed: 21,
        images: 2,
        samples_per_image: 300,
        ..SynthConfig::default()
    });

    let fit_credential = add_credential(worker).await;
    add_images(worker, &fit_credential, &dataset, &[0, 1]).await;
    add_serial_marker(worker, &fit_credential, [1; 32]).await;
    let linked_two = add_credential(worker).await;
    add_serial_marker(worker, &linked_two, [2; 32]).await;
    let linked_three = add_credential(worker).await;
    add_serial_marker(worker, &linked_three, [3; 32]).await;
    let unlinked_one = add_credential(worker).await;
    let unlinked_two = add_credential(worker).await;

    let body = UploadBodyId::new(BODY).expect("body");
    let look = LookSlug::new(LOOK).expect("look");
    let mut expected: Option<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> = None;
    for state in ["none", "owner_reported", "confirmed"] {
        clear_manual_checks(worker, &camera).await;
        match state {
            "none" => {}
            "owner_reported" => {
                insert_manual_check(worker, &unlinked_one, &camera, digest, 10).await;
                insert_manual_check(worker, &unlinked_two, &camera, digest, 11).await;
            }
            "confirmed" => {
                insert_manual_check(worker, &fit_credential, &camera, digest, 20).await;
                insert_manual_check(worker, &linked_two, &camera, digest, 21).await;
                insert_manual_check(worker, &linked_three, &camera, digest, 22).await;
            }
            _ => unreachable!(),
        }
        let summary = looklab_server::manual_summary(worker.pool(), &camera)
            .await
            .expect("manual summary");
        let control = summary
            .control(CapabilityKey::NativeSimulations)
            .expect("control");
        let expected_state = match state {
            "none" => looklab_schema::ConfirmationState::Unconfirmed,
            "owner_reported" => looklab_schema::ConfirmationState::OwnerReported,
            "confirmed" => looklab_schema::ConfirmationState::Confirmed,
            _ => unreachable!(),
        };
        assert_eq!(control.state, expected_state, "{state}");
        if state == "owner_reported" {
            assert_eq!(
                control.firmware[0].owner_profiles, 2,
                "two unlinked profiles count"
            );
        }
        if state == "confirmed" {
            assert_eq!(control.agreeing_groups, 3, "three linked groups confirm");
        }

        let pools = looklab_worker::pool::build_all(
            worker.pool(),
            worker.config(),
            &body,
            &look,
            &looklab_worker::pool::EvidenceScope::Experimental,
        )
        .await
        .expect("pools");
        let (fit, pool, card) = fit_bytes(&pools);
        let matched = match_bytes();
        if let Some(previous) = &expected {
            assert_eq!(&fit, &previous.0, "fit changed at {state}");
            assert_eq!(&pool, &previous.1, "pool changed at {state}");
            assert_eq!(&card, &previous.2, "export changed at {state}");
            assert_eq!(&matched, &previous.3, "match changed at {state}");
        } else {
            expected = Some((fit, pool, card, matched));
        }
    }
}
