// SPDX-License-Identifier: AGPL-3.0-or-later

use assert_cmd::Command;
use looklab_model::file::{LookDocument, LookIdentity};
use looklab_model::{Look, LookKind, WbPolicy};
use looklab_server::admin::{self, ManifestArtifact, VersionStatus};
use looklab_server::{ContributionId, CredentialId};
use looklab_trust::VersionId;
use looklab_trust::{Scope, ScopeRule};
use looklab_upload::{BodyId, LookSlug};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[tokio::test]
async fn admin_grants_revokes_and_lists_through_the_server_library() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");
    let subject = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES (?, x'01', '{}', 0)",
    )
    .bind(subject.as_str())
    .execute(&pool)
    .await
    .expect("credential");

    let path_text = path.to_str().expect("path");
    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "grant",
            "--db",
            path_text,
            "--subject",
            subject.as_str(),
            "--role",
            "admin",
            "--reason",
            "bootstrap",
        ])
        .assert()
        .success();

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "grant",
            "--db",
            path_text,
            "--subject",
            subject.as_str(),
            "--role",
            "seeder",
            "--scope-body",
            "fujifilm/x-t5",
            "--reason",
            "chart sessions",
        ])
        .assert()
        .success();

    let roles: String = sqlx::query_scalar("SELECT roles FROM credentials WHERE id = ?")
        .bind(subject.as_str())
        .fetch_one(&pool)
        .await
        .expect("roles");
    assert_eq!(roles, "[\"admin\"]");
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT tier FROM credentials WHERE id = ?")
            .bind(subject.as_str())
            .fetch_one(&pool)
            .await
            .expect("tier"),
        "seeder"
    );
    let scope: String =
        sqlx::query_scalar("SELECT scope FROM grants WHERE role_or_tier = 'seeder'")
            .fetch_one(&pool)
            .await
            .expect("scope");
    let scope: Scope = serde_json::from_str(&scope).expect("scope json");
    assert!(scope.rules.contains(&ScopeRule::body_all_looks(
        BodyId::new("fujifilm/x-t5").expect("body")
    )));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM audit_log")
            .fetch_one(&pool)
            .await
            .expect("audit rows"),
        2
    );

    let output = Command::cargo_bin("looklab")
        .expect("binary")
        .args(["admin", "list", "--db", path_text])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let listing = String::from_utf8(output).expect("utf-8");
    assert!(listing.contains(subject.as_str()));
    assert!(listing.contains("seeder"));
    assert!(listing.contains("Fujifilm X-T5 · all looks"));

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "revoke",
            "--db",
            path_text,
            "--subject",
            subject.as_str(),
            "--role",
            "seeder",
            "--reason",
            "superseded",
        ])
        .assert()
        .success();
    assert!(
        sqlx::query_scalar::<_, Option<i64>>(
            "SELECT revoked_at FROM grants WHERE role_or_tier = 'seeder'"
        )
        .fetch_one(&pool)
        .await
        .expect("revoked_at")
        .is_some()
    );
}

#[tokio::test]
async fn admin_imports_a_manifest_lut_through_the_cli_actor() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/luts/manifest.toml");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--manifest",
            manifest.to_str().expect("manifest"),
            "--id",
            "leica-classic-gold-200",
        ])
        .assert()
        .success();

    let (status, payload): (String, String) =
        sqlx::query_as("SELECT status, payload FROM proposals WHERE type = 'lut_look'")
            .fetch_one(&pool)
            .await
            .expect("proposal");
    assert_eq!(status, "approved");
    let validated =
        looklab_schema::validate_proposal(looklab_schema::ProposalKind::LutLook, &payload)
            .expect("payload");
    let looklab_schema::ValidatedPayload::LutLook(stored) = validated else {
        panic!("not a LUT proposal");
    };
    assert_eq!(stored.look.as_str(), "leica-classic-gold-200");
    assert_eq!(stored.name, "Leica Classic Gold 200");
    assert_eq!(stored.license.source_license.as_deref(), Some("CC0-1.0"));
    assert_eq!(
        stored.attribution,
        "mistycycles (Discord), donated 2026-11-05"
    );
    let looklab_schema::LutEncoding::Display {
        base:
            base @ looklab_schema::LutBase::FamilyRendering {
                maker,
                rendering,
                dynamic_range,
                ..
            },
    } = &stored.encoding
    else {
        panic!("not a display-referred family rendering");
    };
    assert_eq!(maker.as_str(), "fujifilm");
    assert_eq!(rendering.as_str(), "Eterna");
    assert_eq!(*dynamic_range, Some(looklab_schema::LutDynamicRange::Dr200));
    let reference = looklab_model::file::BaseReference::try_from(base).expect("base reference");
    assert_eq!(
        reference.resolve_status(None),
        looklab_model::file::BaseStatus::BaseIdentifiedUnmeasured
    );
    let moderator: String =
        sqlx::query_scalar("SELECT moderator_credential FROM moderation_decisions")
            .fetch_one(&pool)
            .await
            .expect("decision");
    assert_eq!(moderator, "cli");
    let kind: String = sqlx::query_scalar("SELECT kind FROM jobs")
        .fetch_one(&pool)
        .await
        .expect("job");
    assert_eq!(kind, "lut_import");
}

#[tokio::test]
async fn admin_refuses_a_manifest_lut_with_the_wrong_cube() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/luts");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--manifest",
            data.join("manifest.toml").to_str().expect("manifest"),
            "--id",
            "leica-classic-gold-200",
            "--cube",
            data.join("CineStill_800T_1600T_sRGB_V4.cube")
                .to_str()
                .expect("cube"),
        ])
        .assert()
        .failure();

    for table in ["proposals", "moderation_decisions", "jobs", "audit_log"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .expect("count");
        assert_eq!(count, 0, "{table} has rows");
    }
}

#[tokio::test]
async fn admin_requires_a_flag_for_a_non_original_manifest_entry() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let manifest =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/luts/manifest.toml");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--manifest",
            manifest.to_str().expect("manifest"),
            "--id",
            "ricoh-griv-cinema-green-flog2",
        ])
        .assert()
        .failure();
    let proposals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
        .fetch_one(&pool)
        .await
        .expect("proposals");
    assert_eq!(proposals, 0);

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--manifest",
            manifest.to_str().expect("manifest"),
            "--id",
            "ricoh-griv-cinema-green-flog2",
            "--allow-non-original",
        ])
        .assert()
        .success();
    let payload: String =
        sqlx::query_scalar("SELECT payload FROM proposals WHERE type = 'lut_look'")
            .fetch_one(&pool)
            .await
            .expect("proposal");
    let validated =
        looklab_schema::validate_proposal(looklab_schema::ProposalKind::LutLook, &payload)
            .expect("payload");
    let looklab_schema::ValidatedPayload::LutLook(stored) = validated else {
        panic!("not a LUT proposal");
    };
    assert_eq!(stored.look.as_str(), "ricoh-griv-cinema-green-flog2");
}

#[tokio::test]
async fn admin_imports_an_explicit_lut_with_an_unnamed_renderer() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let cube = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/luts/Leica_Classic_Gold_200.cube");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");
    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--cube",
            cube.to_str().expect("cube"),
            "--name",
            "Explicit Look",
            "--license",
            "CC0-1.0",
            "--attribution",
            "a donor",
            "--encoding",
            "body",
            "--base-body",
            "fujifilm/x-e3",
            "--reason",
            "explicit import",
        ])
        .assert()
        .success();
    let payload: String =
        sqlx::query_scalar("SELECT payload FROM proposals WHERE type = 'lut_look'")
            .fetch_one(&pool)
            .await
            .expect("proposal");
    let validated =
        looklab_schema::validate_proposal(looklab_schema::ProposalKind::LutLook, &payload)
            .expect("payload");
    let looklab_schema::ValidatedPayload::LutLook(stored) = validated else {
        panic!("not a LUT proposal");
    };
    assert_eq!(stored.look.as_str(), "explicit-look");
    let looklab_schema::LutEncoding::Display {
        base:
            looklab_schema::LutBase::BodyRealization {
                renderer,
                film_simulation,
                dynamic_range,
                ..
            },
    } = &stored.encoding
    else {
        panic!("not a body rendering");
    };
    assert_eq!(*renderer, looklab_schema::LutRenderer::Unspecified);
    assert!(film_simulation.is_none());
    assert!(dynamic_range.is_none());

    let second = TempDir::new().expect("temp dir");
    let second_path = second.path().join("looklab.sqlite3");
    let second_blobs = second.path().join("blobs");
    std::fs::create_dir_all(&second_blobs).expect("blob dir");
    let second_pool = looklab_server::open_database(&second_path)
        .await
        .expect("database");
    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "import-lut",
            "--db",
            second_path.to_str().expect("path"),
            "--blobs",
            second_blobs.to_str().expect("blobs"),
            "--cube",
            cube.to_str().expect("cube"),
            "--name",
            "Explicit Look",
            "--license",
            "CC0-1.0",
            "--attribution",
            "a donor",
            "--encoding",
            "body",
            "--base-body",
            "fujifilm/x-e3",
        ])
        .assert()
        .failure();
    let proposals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
        .fetch_one(&second_pool)
        .await
        .expect("proposals");
    assert_eq!(proposals, 0);
}

fn store_blob(blobs: &std::path::Path, bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::write(blobs.join(&hex), bytes).expect("blob");
    hex
}

fn version_artifacts(blobs: &std::path::Path, look: &str, version: u32) -> Vec<ManifestArtifact> {
    let identity = LookIdentity::new(
        look,
        &format!("{version}.0.0"),
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let document = LookDocument::from_look(identity, Look::colorimetric()).expect("document");
    let release = serde_json::json!({
        "version": version,
        "gauge": 0.0,
        "anchor": version,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": {"probes": []},
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": format!("{version}.0.0")},
        "created_at": 0,
    });
    vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: store_blob(blobs, &document.encode().expect("look bytes")),
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: store_blob(blobs, &serde_json::to_vec(&release).expect("release json")),
        },
    ]
}

async fn accept_family_version(db: &sqlx::SqlitePool, look: &str, version: u32) {
    let subject = format!("shared_family:{look}");
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, created_at)
         VALUES (?, 'shared_family', ?, 0)
         ON CONFLICT(key) DO NOTHING",
    )
    .bind(&subject)
    .bind(look)
    .execute(db)
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_subject_revisions (subject_key, revision, role, recorded_at)
         VALUES (?, ?, 'target', 0)
         ON CONFLICT(subject_key, revision) DO NOTHING",
    )
    .bind(&subject)
    .bind(i64::from(version))
    .execute(db)
    .await
    .expect("accepted marker");
}

#[tokio::test]
async fn admin_reverts_a_look_through_the_cli_actor() {
    const LOOK: &str = "classic-chrome";
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");
    let contributor = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', 'new', '[]', 0.0, 0, 'active')",
    )
    .bind(contributor.as_str())
    .bind(contributor.as_str().as_bytes())
    .execute(&pool)
    .await
    .expect("credential");
    let contribution = ContributionId::new();
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, intent, target_look_id,
          body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', 'library', 'native', ?, 'fujifilm/x-t5', '2.10',
                 'sim', '{}', 'accepted', 0)",
    )
    .bind(contribution.as_str())
    .bind(contributor.as_str())
    .bind(LOOK)
    .execute(&pool)
    .await
    .expect("contribution");
    accept_family_version(&pool, LOOK, 1).await;
    admin::write_version_manifest(
        &pool,
        &LookSlug::try_from(LOOK).expect("slug"),
        VersionId::new(1),
        VersionStatus::Published,
        &version_artifacts(&blobs, LOOK, 1),
        &[],
    )
    .await
    .expect("version 1");

    Command::cargo_bin("looklab")
        .expect("binary")
        .args([
            "admin",
            "revert",
            "--db",
            path.to_str().expect("path"),
            "--blobs",
            blobs.to_str().expect("blobs"),
            "--look",
            LOOK,
            "--version",
            "1",
            "--reject-contribution",
            contribution.as_str(),
            "--reason",
            "the contribution mislabels the look",
        ])
        .assert()
        .success();

    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
        .bind(LOOK)
        .fetch_one(&pool)
        .await
        .expect("versions");
    assert_eq!(versions, 2);
    let successor: String =
        sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ? AND version = 2")
            .bind(LOOK)
            .fetch_one(&pool)
            .await
            .expect("successor status");
    assert_eq!(successor, "deprecated");
    let rejected: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(contribution.as_str())
        .fetch_one(&pool)
        .await
        .expect("contribution status");
    assert_eq!(rejected, "quarantined");
    let audit: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'revert' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .expect("audit row");
    let audit: serde_json::Value = serde_json::from_str(&audit).expect("audit json");
    assert_eq!(audit["actor"], "cli");
    assert_eq!(audit["look"], LOOK);
    assert_eq!(
        audit["plan"]["rejected_contributions"],
        serde_json::json!([contribution.as_str()])
    );
    assert!(
        audit["plan"]["restoration"]
            .get("historical_needs_rebuild")
            .is_some(),
        "the successor is a held restoration"
    );
    let refit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(&pool)
    .await
    .expect("refit job");
    assert_eq!(refit, 1);
}

#[tokio::test]
async fn admin_blocks_future_intake_through_the_cli_actor_once() {
    let dir = TempDir::new().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let pool = looklab_server::open_database(&path)
        .await
        .expect("database");
    let profile = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES (?, x'01', '{}', 0)",
    )
    .bind(profile.as_str())
    .execute(&pool)
    .await
    .expect("credential");

    for _ in 0..2 {
        Command::cargo_bin("looklab")
            .expect("binary")
            .args([
                "admin",
                "block-intake",
                "--db",
                path.to_str().expect("path"),
                "--profile",
                profile.as_str(),
                "--reason",
                "repeated mislabelled uploads",
            ])
            .assert()
            .success();
    }

    let blocks: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM credential_intake_blocks WHERE credential_id = ?")
            .bind(profile.as_str())
            .fetch_one(&pool)
            .await
            .expect("blocks");
    assert_eq!(blocks, 1);
    let audits: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM audit_log WHERE type = 'credential_intake_blocked'",
    )
    .fetch_one(&pool)
    .await
    .expect("audit rows");
    assert_eq!(audits, 1);
    let audit: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'credential_intake_blocked'",
    )
    .fetch_one(&pool)
    .await
    .expect("audit row");
    let audit: serde_json::Value = serde_json::from_str(&audit).expect("audit json");
    assert_eq!(audit["actor"], "cli");
    assert_eq!(audit["subject"], profile.as_str());
}
