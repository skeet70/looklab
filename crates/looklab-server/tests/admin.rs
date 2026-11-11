// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};

use looklab_export::{CubeFile, Lut3d, write_cube};
use looklab_model::file::{LookIdentity, LookKind, WbPolicy};
use looklab_model::{Look, LookDocument};
use looklab_schema::{
    Applicability, ArtifactRole, ContributionRef, DependencyKind, DependencyRef, DependencyTarget,
    EligibilitySnapshotId, FamilyTargetId, ImageRef, InvalidationReason, LicenseDeclaration,
    LookId, LutBase, LutDynamicRange, LutEncoding, LutFile, LutLookProposal, LutRenderer,
    MANUAL_CATALOG_VERSION, ManualCatalog, ModelSubject, ProposalKind, PublishedTargetId,
    SafetyCheck, Sha256Hex, SourceRef, SubjectKey, TargetBasis, TargetEligibility,
    TargetEligibilityStatus, TargetVersion, ValidatedPayload,
};
use looklab_server::admin::{
    self, AdminError, ManifestArtifact, Restoration, RestorationHold, WorkerProposal,
};
use looklab_server::model_store::{self, JobLease, NumericalSources};
use looklab_server::{
    ContributionId, CredentialId, EndorsementId, FsBlobStore, GrantId, JobId, LookVersionId,
    LutAdmission, ProposalError, Tier, admit_lut,
};
use looklab_trust::{Actor, Grant, Scope, ScopeRule, TrustError, VersionId};
use looklab_upload::{BodyId, LookSlug};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    db: SqlitePool,
    blobs: PathBuf,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let db = looklab_server::open_database(&dir.path().join("looklab.sqlite3"))
        .await
        .expect("database");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    Fixture {
        _dir: dir,
        db,
        blobs,
    }
}

fn store_blob(blobs: &Path, bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    std::fs::write(blobs.join(&hex), bytes).expect("blob");
    hex
}

fn version_artifacts(
    blobs: &Path,
    look: &str,
    version: u32,
    anchor: u32,
    union: serde_json::Value,
) -> Vec<ManifestArtifact> {
    let identity = LookIdentity::new(
        look,
        &format!("{version}.0.0"),
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let document = LookDocument::from_look(identity, Look::colorimetric()).expect("document");
    let look_bytes = document.encode().expect("look bytes");
    let release = serde_json::json!({
        "version": version,
        "gauge": 0.0,
        "anchor": anchor,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": union,
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": format!("{anchor}.0.0")},
        "created_at": 0,
    });
    vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: store_blob(blobs, &look_bytes),
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: store_blob(blobs, &serde_json::to_vec(&release).expect("release json")),
        },
    ]
}

fn zero_support_version_artifacts(
    blobs: &Path,
    look: &str,
    version: u32,
    anchor: u32,
) -> Vec<ManifestArtifact> {
    use looklab_model::file::{Density, ProbeCovariance, ProbeScalars, UncertaintyData};

    let identity = LookIdentity::new(
        look,
        &format!("{version}.0.0"),
        LookKind::NativeFamily,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let count = looklab_model::probes::v1().probes.len();
    let mut covariance = Vec::with_capacity(count * 9);
    for _ in 0..count {
        covariance.extend_from_slice(&[0.01, 0.0, 0.0, 0.0, 0.01, 0.0, 0.0, 0.0, 0.01]);
    }
    let document = LookDocument::from_look(identity, Look::colorimetric())
        .expect("document")
        .with_uncertainty(UncertaintyData {
            probe_cov: Some(ProbeCovariance::new(count, covariance.clone()).expect("covariance")),
            probe_cov_post: Some(ProbeCovariance::new(count, covariance).expect("covariance")),
            support: Some(ProbeScalars::new(vec![0.0; count]).expect("support")),
            density: Some(Density::new(vec![1.0; count]).expect("density")),
            ..UncertaintyData::default()
        })
        .expect("uncertainty");
    let look_bytes = document.encode().expect("look bytes");
    let release = serde_json::json!({
        "version": version,
        "gauge": 0.0,
        "anchor": anchor,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": {"probes": []},
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": format!("{anchor}.0.0")},
        "created_at": 0,
    });
    vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: store_blob(blobs, &look_bytes),
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: store_blob(blobs, &serde_json::to_vec(&release).expect("release json")),
        },
    ]
}

fn lut_version_artifacts(
    blobs: &Path,
    look: &str,
    version: u32,
    anchor: u32,
) -> Vec<ManifestArtifact> {
    use looklab_model::file::{
        DeclaredEncoding, Gamut, LicenseRecord, LogTransfer, LutData, LutDefinition, LutRecord,
        OutputEncoding,
    };

    let identity = LookIdentity::new(
        look,
        &format!("{version}.0.0"),
        LookKind::LutDefined,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let definition = LutDefinition {
        name: "Scene Look".to_owned(),
        license: LicenseRecord {
            own_work: false,
            source_license: Some("CC0-1.0".to_owned()),
            source_url: None,
            attribution: "a donor".to_owned(),
        },
        encoding: DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        },
        declared_base: None,
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: 2,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
        },
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let lut = LutData::new(
        2,
        [0.0; 3],
        [1.0; 3],
        (0..24).map(|value| value as f64 / 23.0).collect(),
    )
    .expect("lut");
    let document =
        LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut)
            .expect("document");
    let look_bytes = document.encode().expect("look bytes");
    let release = serde_json::json!({
        "version": version,
        "gauge": 0.0,
        "anchor": anchor,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": {"probes": []},
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": format!("{anchor}.0.0")},
        "created_at": 0,
    });
    vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: store_blob(blobs, &look_bytes),
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: store_blob(blobs, &serde_json::to_vec(&release).expect("release json")),
        },
    ]
}

fn unpinned_display_version_artifacts(
    blobs: &Path,
    look: &str,
    version: u32,
    anchor: u32,
) -> Vec<ManifestArtifact> {
    use looklab_model::file::{
        BaseReference, BaseStatus, DeclaredEncoding, LicenseRecord, LutData, LutDefinition,
        LutRecord, OutputEncoding,
    };

    let identity = LookIdentity::new(
        look,
        &format!("{version}.0.0"),
        LookKind::LutDefined,
        WbPolicy::Adaptive,
    )
    .expect("identity");
    let definition = LutDefinition {
        name: "Display Look".to_owned(),
        license: LicenseRecord {
            own_work: false,
            source_license: Some("CC0-1.0".to_owned()),
            source_url: None,
            attribution: "a donor".to_owned(),
        },
        encoding: DeclaredEncoding::DisplayReferred {
            base: BaseReference::neutral_baseline(),
            status: BaseStatus::BaseIdentifiedUnmeasured,
        },
        declared_base: None,
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: 2,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
        },
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let lut = LutData::new(
        2,
        [0.0; 3],
        [1.0; 3],
        (0..24).map(|value| value as f64 / 23.0).collect(),
    )
    .expect("lut");
    let document =
        LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut)
            .expect("document");
    let look_bytes = document.encode().expect("look bytes");
    let release = serde_json::json!({
        "version": version,
        "gauge": 0.0,
        "anchor": anchor,
        "clip_k": 1.0,
        "probes": [],
        "gated_since_anchor": {"probes": []},
        "converged": true,
        "gate": {},
        "provenance": {"anchor_version": format!("{anchor}.0.0")},
        "created_at": 0,
    });
    vec![
        ManifestArtifact {
            name: looklab_publish::DOCUMENT_ARTIFACT.to_owned(),
            sha256: store_blob(blobs, &look_bytes),
        },
        ManifestArtifact {
            name: "release.json".to_owned(),
            sha256: store_blob(blobs, &serde_json::to_vec(&release).expect("release json")),
        },
    ]
}

async fn credential(db: &SqlitePool, tier: &str, roles: &str, reputation: f64) -> CredentialId {
    let id = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', ?, ?, ?, 0, 'active')",
    )
    .bind(id.as_str())
    .bind(id.as_str().as_bytes())
    .bind(tier)
    .bind(roles)
    .bind(reputation)
    .execute(db)
    .await
    .expect("credential");
    id
}

async fn add_contribution(
    db: &SqlitePool,
    credential: &CredentialId,
    body: &str,
    kind: &str,
    target_look: Option<&str>,
) -> ContributionId {
    add_contribution_with_intent(db, credential, body, kind, target_look, "native").await
}

async fn add_contribution_with_intent(
    db: &SqlitePool,
    credential: &CredentialId,
    body: &str,
    kind: &str,
    target_look: Option<&str>,
    intent: &str,
) -> ContributionId {
    let id = ContributionId::new();
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, intent, target_look_id,
          body_id, firmware_version, sim_key, settings, status, created_at)
         VALUES (?, ?, x'00', x'00', 'pow', ?, ?, ?, ?, '2.10', 'sim', '{}', 'pending', 0)",
    )
    .bind(id.as_str())
    .bind(credential.as_str())
    .bind(kind)
    .bind(intent)
    .bind(target_look)
    .bind(body)
    .execute(db)
    .await
    .expect("contribution");
    id
}

fn actor(id: &CredentialId) -> Actor {
    Actor::Credential(admin::trust_id(id).expect("trust id"))
}

fn body(value: &str) -> BodyId {
    BodyId::new(value).expect("body")
}

fn look(value: &str) -> LookSlug {
    LookSlug::try_from(value).expect("look")
}

async fn accept_family_version(db: &SqlitePool, look: &LookSlug, version: u32) {
    let subject = format!("shared_family:{}", look.as_str());
    sqlx::query(
        "INSERT INTO model_subjects (key, kind, family, created_at)
         VALUES (?, 'shared_family', ?, 0)
         ON CONFLICT(key) DO NOTHING",
    )
    .bind(&subject)
    .bind(look.as_str())
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

fn rule(body: Option<&str>, look: Option<&str>) -> ScopeRule {
    ScopeRule::new(body.map(self::body), look.map(self::look))
}

fn scope(rules: Vec<ScopeRule>) -> Scope {
    Scope::new(rules)
}

fn body_scope(body: &str) -> Scope {
    scope(vec![rule(Some(body), None)])
}

fn look_scope(body: &str, look: &str) -> Scope {
    scope(vec![rule(Some(body), Some(look))])
}

const BODY_A: &str = "canon/eos-r5";
const BODY_B: &str = "fujifilm/x-t5";
const LOOK_1: &str = "classic-chrome";
const LOOK_2: &str = "reala-ace";

#[tokio::test]
async fn card_intent_is_not_root_admissible() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let card = add_contribution_with_intent(db, &contributor, BODY_B, "chart", None, "card").await;

    let error = admin::admit_root(
        db,
        actor(&seeder),
        &card,
        body_scope(BODY_B),
        "card sessions are not root data",
    )
    .await
    .expect_err("card intent is ineligible");
    assert!(matches!(error, AdminError::IneligibleContribution));
}

#[tokio::test]
async fn seeder_scope_comes_from_grants_not_endorsements() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let admin = credential(db, "new", "[\"admin\"]", 0.5).await;
    let issuer = credential(db, "seeder", "[]", 1.2).await;
    let candidate = credential(db, "new", "[]", 0.5).await;
    let other = credential(db, "new", "[]", 0.5).await;

    admin::endorse(
        db,
        actor(&issuer),
        &candidate,
        body_scope(BODY_B),
        "broad endorsement",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    admin::grant_credential(
        db,
        actor(&admin),
        &candidate,
        Grant::Seeder,
        Some(look_scope(BODY_B, LOOK_1)),
        "narrow seeder grant",
    )
    .await
    .expect("seeder grant");

    let error = admin::endorse(
        db,
        actor(&candidate),
        &other,
        look_scope(BODY_B, LOOK_2),
        "outside the grant",
        Vec::new(),
    )
    .await
    .expect_err("the endorsement scope must not widen the seeder grant");
    assert!(matches!(
        error,
        AdminError::Trust(TrustError::ScopeNotWithinIssuer)
    ));

    let contribution = add_contribution(db, &issuer, BODY_B, "chart", None).await;
    let error = admin::admit_root(
        db,
        actor(&candidate),
        &contribution,
        look_scope(BODY_B, LOOK_2),
        "outside the grant",
    )
    .await
    .expect_err("out-of-scope root admission");
    assert!(matches!(error, AdminError::IneligibleContribution));

    admin::admit_root(
        db,
        actor(&candidate),
        &contribution,
        look_scope(BODY_B, LOOK_1),
        "inside the grant",
    )
    .await
    .expect("in-scope root admission");
}

#[tokio::test]
async fn transitions_write_only_the_fields_they_change() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let admin = credential(db, "new", "[\"admin\"]", 0.5).await;
    let issuer = credential(db, "seeder", "[]", 1.2).await;
    let subject = credential(db, "new", "[]", 0.9).await;
    admin::grant_credential(
        db,
        actor(&admin),
        &subject,
        Grant::Admin,
        None,
        "temporary admin",
    )
    .await
    .expect("admin grant");

    admin::endorse(
        db,
        actor(&issuer),
        &subject,
        body_scope(BODY_A),
        "reviewed",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    let (roles, reputation): (String, f64) =
        sqlx::query_as("SELECT roles, reputation FROM credentials WHERE id = ?")
            .bind(subject.as_str())
            .fetch_one(db)
            .await
            .expect("credential");
    assert_eq!(roles, "[\"admin\"]");
    assert!((reputation - 0.9).abs() < f64::EPSILON);

    let (endorsed, revoked) = tokio::join!(
        admin::endorse(
            db,
            actor(&issuer),
            &subject,
            body_scope(BODY_A),
            "again",
            Vec::new(),
        ),
        admin::revoke_grant(db, actor(&admin), &subject, Grant::Admin, "demoted"),
    );
    endorsed.expect("endorsement");
    revoked.expect("revocation");
    let roles: String = sqlx::query_scalar("SELECT roles FROM credentials WHERE id = ?")
        .bind(subject.as_str())
        .fetch_one(db)
        .await
        .expect("roles");
    assert_eq!(roles, "[]");
    let tier: String = sqlx::query_scalar("SELECT tier FROM credentials WHERE id = ?")
        .bind(subject.as_str())
        .fetch_one(db)
        .await
        .expect("tier");
    assert_eq!(tier, "endorsed");
}

#[tokio::test]
async fn root_admission_uses_the_contribution_target_and_scoped_standing() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contributor = credential(db, "new", "[]", 0.5).await;
    admin::endorse(
        db,
        actor(&seeder),
        &contributor,
        body_scope(BODY_A),
        "body A only",
        Vec::new(),
    )
    .await
    .expect("endorsement");

    let contribution = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    let error = admin::admit_root(
        db,
        actor(&seeder),
        &contribution,
        look_scope(BODY_B, LOOK_1),
        "outside the contributor's standing",
    )
    .await
    .expect_err("a contributor endorsed for another body has no standing here");
    assert!(matches!(error, AdminError::IneligibleContribution));

    admin::endorse(
        db,
        actor(&seeder),
        &contributor,
        body_scope(BODY_B),
        "body B",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    let targeted = add_contribution(db, &contributor, BODY_B, "chart", Some(LOOK_1)).await;
    let error = admin::admit_root(
        db,
        actor(&seeder),
        &targeted,
        look_scope(BODY_B, LOOK_2),
        "wrong look",
    )
    .await
    .expect_err("the contribution targets a different look");
    assert!(matches!(error, AdminError::IneligibleContribution));
    admin::admit_root(
        db,
        actor(&seeder),
        &targeted,
        look_scope(BODY_B, LOOK_1),
        "right look",
    )
    .await
    .expect("matching target");

    let body_target = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    let error = admin::admit_root(
        db,
        actor(&seeder),
        &body_target,
        body_scope(BODY_A),
        "wrong body",
    )
    .await
    .expect_err("the contribution belongs to another body");
    assert!(matches!(error, AdminError::IneligibleContribution));

    let gradient = add_contribution(db, &contributor, BODY_B, "gradient", None).await;
    let error = admin::admit_root(
        db,
        actor(&seeder),
        &gradient,
        body_scope(BODY_B),
        "gradient cannot root a body",
    )
    .await
    .expect_err("only chart sessions root a body input transform");
    assert!(matches!(error, AdminError::IneligibleContribution));
    admin::admit_root(
        db,
        actor(&seeder),
        &body_target,
        body_scope(BODY_B),
        "chart roots the body",
    )
    .await
    .expect("body root");
}

#[tokio::test]
async fn admitting_root_queues_a_refit() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contributor = credential(db, "new", "[]", 0.5).await;
    admin::endorse(
        db,
        actor(&seeder),
        &contributor,
        body_scope(BODY_B),
        "body B",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    let contribution = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    admin::admit_root(
        db,
        actor(&seeder),
        &contribution,
        body_scope(BODY_B),
        "new chart",
    )
    .await
    .expect("admit");

    let kind: String = sqlx::query_scalar(
        "SELECT kind FROM jobs WHERE kind = 'nightly_refit'
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(db)
    .await
    .expect("refit job");
    assert_eq!(kind, "nightly_refit");
}

#[tokio::test]
async fn a_running_nightly_refit_does_not_swallow_a_root_request() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contributor = credential(db, "new", "[]", 0.5).await;
    admin::endorse(
        db,
        actor(&seeder),
        &contributor,
        body_scope(BODY_B),
        "body B",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, 'nightly_refit', '{}', 'running', 1, 0, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .execute(db)
    .await
    .expect("running refit");

    let contribution = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    admin::admit_root(
        db,
        actor(&seeder),
        &contribution,
        body_scope(BODY_B),
        "new chart",
    )
    .await
    .expect("admit");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'nightly_refit' AND status = 'queued'"
        )
        .fetch_one(db)
        .await
        .expect("queued refits"),
        1,
        "a request that arrives while a refit runs must be queued"
    );

    let second = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    admin::admit_root(
        db,
        actor(&seeder),
        &second,
        body_scope(BODY_B),
        "second chart",
    )
    .await
    .expect("second admit");
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM jobs WHERE kind = 'nightly_refit' AND status = 'queued'"
        )
        .fetch_one(db)
        .await
        .expect("queued refits"),
        1,
        "a queued refit coalesces later requests"
    );
}

#[tokio::test]
async fn revocations_are_bounded_by_the_revoker_scope() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let admin = credential(db, "new", "[\"admin\"]", 0.5).await;
    let seeder_a = credential(db, "new", "[]", 0.5).await;
    let seeder_b = credential(db, "new", "[]", 0.5).await;
    for (subject, scope) in [
        (&seeder_a, body_scope(BODY_A)),
        (&seeder_b, body_scope(BODY_B)),
    ] {
        admin::grant_credential(
            db,
            actor(&admin),
            subject,
            Grant::Seeder,
            Some(scope),
            "scoped seeder",
        )
        .await
        .expect("seeder grant");
    }
    let subject = credential(db, "new", "[]", 0.5).await;
    let endorsement = admin::endorse(
        db,
        actor(&seeder_b),
        &subject,
        body_scope(BODY_B),
        "body B",
        Vec::new(),
    )
    .await
    .expect("endorsement");

    let error = admin::revoke_endorsement(db, actor(&seeder_a), &subject, "out of scope")
        .await
        .expect_err("out-of-scope endorsement revocation");
    assert!(matches!(error, AdminError::NotAuthorized));
    let revoked = admin::revoke_endorsement(db, actor(&seeder_b), &subject, "issuer")
        .await
        .expect("issuer revocation");
    assert_eq!(revoked.ids, vec![endorsement.id]);

    let contribution = add_contribution(db, &seeder_b, BODY_B, "chart", None).await;
    let root = admin::admit_root(
        db,
        actor(&seeder_b),
        &contribution,
        look_scope(BODY_B, LOOK_1),
        "root",
    )
    .await
    .expect("root admission");
    let error = admin::revoke_root(db, actor(&seeder_a), &root.id, "out of scope")
        .await
        .expect_err("out-of-scope root revocation");
    assert!(matches!(error, AdminError::NotAuthorized));
    admin::revoke_root(db, actor(&seeder_b), &root.id, "issuer")
        .await
        .expect("issuer root revocation");

    let second = admin::admit_root(
        db,
        actor(&seeder_b),
        &contribution,
        look_scope(BODY_B, LOOK_2),
        "root",
    )
    .await
    .expect("root admission");
    admin::revoke_root(db, actor(&admin), &second.id, "admin override")
        .await
        .expect("admins revoke globally");
}

#[tokio::test]
async fn wildcard_root_revocation_creates_versions_for_dependent_looks() {
    use looklab_server::admin::ManifestArtifact;

    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contribution = add_contribution(db, &seeder, BODY_B, "chart", None).await;
    let root = admin::admit_root(
        db,
        actor(&seeder),
        &contribution,
        body_scope(BODY_B),
        "wildcard root",
    )
    .await
    .expect("root admission");

    let artifact = ManifestArtifact {
        name: "look.json".to_owned(),
        sha256: "ab".repeat(32),
    };
    let manifest = admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        std::slice::from_ref(&artifact),
        std::slice::from_ref(&root.id),
    )
    .await
    .expect("manifest");
    let version_id: String =
        sqlx::query_scalar("SELECT id FROM look_versions WHERE look = ? AND version = ?")
            .bind(LOOK_1)
            .bind(manifest.version)
            .fetch_one(db)
            .await
            .expect("version");

    let revocation = admin::revoke_root(db, actor(&seeder), &root.id, "revoked")
        .await
        .expect("revocation");
    assert!(
        revocation
            .new_versions
            .iter()
            .any(|version| { version.look.as_str() == LOOK_1 && version.version == 2 })
    );
    assert!(
        revocation
            .regating
            .contains(&LookVersionId::try_from(version_id).expect("version id"))
    );

    let mixed = add_contribution(db, &seeder, BODY_B, "chart", None).await;
    let root = admin::admit_root(
        db,
        actor(&seeder),
        &mixed,
        scope(vec![
            rule(Some(BODY_B), Some(LOOK_2)),
            rule(Some(BODY_B), None),
        ]),
        "mixed wildcard root",
    )
    .await
    .expect("root admission");
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(2),
        admin::VersionStatus::Published,
        std::slice::from_ref(&artifact),
        std::slice::from_ref(&root.id),
    )
    .await
    .expect("manifest");
    let revocation = admin::revoke_root(db, actor(&seeder), &root.id, "revoked")
        .await
        .expect("revocation");
    let looks: std::collections::BTreeSet<&str> = revocation
        .new_versions
        .iter()
        .map(|version| version.look.as_str())
        .collect();
    assert!(looks.contains(LOOK_1), "dependent look");
    assert!(looks.contains(LOOK_2), "explicit look");
}

#[tokio::test]
async fn a_manifest_keeps_the_requested_status_and_an_existing_row_wins() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let look = look(LOOK_1);
    let artifacts = vec![ManifestArtifact {
        name: "look.looklab".to_owned(),
        sha256: "ab".repeat(32),
    }];
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, 1, 'provisional', NULL, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(LOOK_1)
    .execute(db)
    .await
    .expect("provisional version");

    let preserved = admin::write_version_manifest(
        db,
        &look,
        VersionId::new(1),
        admin::VersionStatus::Rooted,
        &artifacts,
        &[],
    )
    .await
    .expect("manifest for an existing version");
    assert_eq!(preserved.status, admin::VersionStatus::Provisional);

    let rooted = admin::write_version_manifest(
        db,
        &look,
        VersionId::new(2),
        admin::VersionStatus::Rooted,
        &artifacts,
        &[],
    )
    .await
    .expect("rooted manifest");
    assert_eq!(rooted.status, admin::VersionStatus::Rooted);

    let first = admin::manifest_record(db, &look, VersionId::new(1))
        .await
        .expect("first record");
    assert_eq!(first.status, admin::VersionStatus::Provisional);
    let second = admin::manifest_record(db, &look, VersionId::new(2))
        .await
        .expect("second record");
    assert_eq!(second.status, admin::VersionStatus::Rooted);
}

#[tokio::test]
async fn reanchor_requires_a_moderator_and_a_released_version_and_is_audited() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let outsider = credential(db, "new", "[]", 0.5).await;
    let look = look(LOOK_1);
    accept_family_version(db, &look, 1).await;
    admin::write_version_manifest(
        db,
        &look,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &[ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256: "ab".repeat(32),
        }],
        &[],
    )
    .await
    .expect("version 1");
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, 2, 'provisional', NULL, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(LOOK_1)
    .execute(db)
    .await
    .expect("version 2");
    let error = admin::reanchor(db, actor(&outsider), &look, 1, "new root session")
        .await
        .expect_err("outsider");
    assert!(matches!(error, AdminError::NotAuthorized));
    let error = admin::reanchor(db, actor(&moderator), &look, 2, "new root session")
        .await
        .expect_err("provisional");
    assert!(matches!(error, AdminError::VersionNotPublished));
    let error = admin::reanchor(db, actor(&moderator), &look, 9, "new root session")
        .await
        .expect_err("unknown");
    assert!(matches!(error, AdminError::UnknownVersion));
    let record = admin::reanchor(db, actor(&moderator), &look, 1, "new root session")
        .await
        .expect("reanchor");
    assert_eq!(record.version, 1);
    assert_eq!(record.reason, "new root session");
    let (kind, payload): (String, String) =
        sqlx::query_as("SELECT type, payload FROM audit_log ORDER BY seq DESC LIMIT 1")
            .fetch_one(db)
            .await
            .expect("audit row");
    assert_eq!(kind, "re_anchor");
    let payload: serde_json::Value = serde_json::from_str(&payload).expect("payload");
    assert_eq!(payload["look"], LOOK_1);
    assert_eq!(payload["version"], 1);
}

#[tokio::test]
async fn a_revoked_root_cannot_back_a_new_manifest() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let seeder = credential(db, "seeder", "[]", 1.2).await;
    let contributor = credential(db, "new", "[]", 0.5).await;
    admin::endorse(
        db,
        actor(&seeder),
        &contributor,
        body_scope(BODY_B),
        "body B",
        Vec::new(),
    )
    .await
    .expect("endorsement");
    let contribution = add_contribution(db, &contributor, BODY_B, "chart", None).await;
    let root = admin::admit_root(
        db,
        actor(&seeder),
        &contribution,
        body_scope(BODY_B),
        "chart root",
    )
    .await
    .expect("root admission");
    admin::revoke_root(db, actor(&seeder), &root.id, "root withdrawn")
        .await
        .expect("revocation");

    let error = admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Rooted,
        &[ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256: "ab".repeat(32),
        }],
        std::slice::from_ref(&root.id),
    )
    .await
    .expect_err("revoked root");
    assert!(matches!(error, AdminError::RootRevoked(_)));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM look_versions WHERE look = ?")
            .bind(LOOK_1)
            .fetch_one(db)
            .await
            .expect("versions"),
        0
    );
}

#[tokio::test]
async fn endorsement_nominations_are_seeder_only() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let nominated = credential(db, "new", "[]", 1.4).await;

    let catalog = ManualCatalog {
        catalog_version: MANUAL_CATALOG_VERSION,
        cameras: Vec::new(),
    };
    let queue = admin::mod_queue(db, Tier::New, &catalog)
        .await
        .expect("queue");
    assert!(queue.nominations.is_empty());

    let queue = admin::mod_queue(db, Tier::Seeder, &catalog)
        .await
        .expect("queue");
    assert_eq!(queue.nominations.len(), 1);
    assert_eq!(queue.nominations[0].credential, nominated);
}

#[tokio::test]
async fn public_lists_page_with_a_hard_cap() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let admin = credential(db, "new", "[\"admin\"]", 0.5).await;
    for index in 0..3 {
        let subject = credential(db, "new", "[]", 0.5).await;
        admin::grant_credential(
            db,
            actor(&admin),
            &subject,
            Grant::Moderator,
            None,
            &format!("moderator {index}"),
        )
        .await
        .expect("grant");
    }
    let first = admin::list_grants(db, None, 2).await.expect("page");
    assert_eq!(first.grants.len(), 2);
    let cursor: GrantId = first.next_after.expect("cursor");
    let second = admin::list_grants(db, Some(&cursor), 2)
        .await
        .expect("page");
    assert_eq!(second.grants.len(), 1);
    assert!(second.next_after.is_none());
    let ids: std::collections::BTreeSet<GrantId> = first
        .grants
        .iter()
        .chain(second.grants.iter())
        .map(|grant| grant.id.clone())
        .collect();
    assert_eq!(ids.len(), 3);

    let issuer = credential(db, "seeder", "[]", 1.2).await;
    for index in 0..3 {
        let subject = credential(db, "new", "[]", 0.5).await;
        admin::endorse(
            db,
            actor(&issuer),
            &subject,
            body_scope(BODY_A),
            &format!("endorsement {index}"),
            Vec::new(),
        )
        .await
        .expect("endorsement");
    }
    let first = admin::list_endorsements(db, None, 2).await.expect("page");
    assert_eq!(first.endorsements.len(), 2);
    let cursor: EndorsementId = first.next_after.expect("cursor");
    let second = admin::list_endorsements(db, Some(&cursor), 2)
        .await
        .expect("page");
    assert_eq!(second.endorsements.len(), 1);
    assert!(second.next_after.is_none());
}

#[tokio::test]
async fn revert_republishes_the_target_and_rejects_only_named_contributions() {
    use looklab_server::admin::VersionStatus;

    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let bad = add_contribution(db, &contributor, "fujifilm/x-t5", "library", Some(LOOK_1)).await;
    let good = add_contribution(db, &contributor, "fujifilm/x-t5", "library", Some(LOOK_1)).await;
    for contribution in [&bad, &good] {
        sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
            .bind(contribution.as_str())
            .execute(db)
            .await
            .expect("accept contribution");
    }
    sqlx::query(
        "INSERT INTO contribution_images
         (id, contribution_id, samples_hash, n_samples, meta, status)
         VALUES (?, ?, x'00', 1, '{}', 'accepted')",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(bad.as_str())
    .execute(db)
    .await
    .expect("rejected image");
    accept_family_version(db, &look(LOOK_1), 1).await;
    accept_family_version(db, &look(LOOK_1), 2).await;

    let first = version_artifacts(
        &fixture.blobs,
        LOOK_1,
        1,
        1,
        serde_json::json!({"probes": ["v1"]}),
    );
    let second = version_artifacts(
        &fixture.blobs,
        LOOK_1,
        2,
        2,
        serde_json::json!({"probes": ["current"]}),
    );
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &first,
        &[],
    )
    .await
    .expect("version 1");
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(2),
        admin::VersionStatus::Published,
        &second,
        &[],
    )
    .await
    .expect("version 2");
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES ('re_anchor', ?, 0)")
        .bind(
            serde_json::json!({
                "event": "re_anchor",
                "look": LOOK_1,
                "version": 9,
            })
            .to_string(),
        )
        .execute(db)
        .await
        .expect("stale re-anchor");

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&bad),
        "contributor introduced a hue shift",
    )
    .await
    .expect("revert");

    assert_eq!(reverted.target_version, 1);
    assert_eq!(reverted.new_version, 3);
    assert_eq!(reverted.rejected, vec![bad.clone()]);
    assert_eq!(
        reverted.restoration,
        Restoration::HistoricalNeedsRebuild {
            reason: RestorationHold::DependencyHistoryUnavailable
        }
    );

    let target = admin::manifest_record(db, &look(LOOK_1), VersionId::new(1))
        .await
        .expect("target manifest");
    let published = admin::manifest_record(db, &look(LOOK_1), VersionId::new(3))
        .await
        .expect("new manifest");
    assert_eq!(published.status, VersionStatus::Deprecated);
    assert_eq!(
        published
            .artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>(),
        target
            .artifacts
            .iter()
            .map(|artifact| artifact.name.as_str())
            .collect::<Vec<_>>()
    );
    assert_ne!(published.artifacts, target.artifacts);
    let look_hash = published
        .artifacts
        .iter()
        .find(|artifact| artifact.name == looklab_publish::DOCUMENT_ARTIFACT)
        .expect("look artifact")
        .sha256
        .clone();
    let look_bytes = std::fs::read(fixture.blobs.join(look_hash)).expect("republished look");
    let document = LookDocument::decode(&look_bytes).expect("republished document");
    assert_eq!(document.file.look.version.as_str(), "3.0.0");
    assert_eq!(document.file.provenance.root.anchor_version, "2.0.0");
    let release_hash = published
        .artifacts
        .iter()
        .find(|artifact| artifact.name == "release.json")
        .expect("release artifact")
        .sha256
        .clone();
    let release: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.blobs.join(release_hash)).expect("release"))
            .expect("release json");
    assert_eq!(release["version"], 3);
    assert_eq!(release["anchor"], 2);
    assert_eq!(
        release["gated_since_anchor"],
        serde_json::json!({"probes": ["current"]})
    );
    let old = admin::manifest_record(db, &look(LOOK_1), VersionId::new(2))
        .await
        .expect("old manifest");
    assert_eq!(old.artifacts, second);
    let original = admin::manifest_record(db, &look(LOOK_1), VersionId::new(1))
        .await
        .expect("original manifest");
    assert_eq!(original.artifacts, first);

    let rejected: Vec<String> =
        sqlx::query_scalar("SELECT contribution_id FROM contribution_rejections")
            .fetch_all(db)
            .await
            .expect("rejections");
    assert_eq!(rejected, vec![bad.as_str().to_owned()]);
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM contributions WHERE id IN (?, ?) ORDER BY id")
            .bind(bad.as_str())
            .bind(good.as_str())
            .fetch_all(db)
            .await
            .expect("statuses");
    assert!(
        statuses
            .iter()
            .any(|(id, status)| id == bad.as_str() && status == "quarantined")
    );
    assert!(
        statuses
            .iter()
            .any(|(id, status)| id == good.as_str() && status == "accepted")
    );
    for (contribution, expected) in [(&bad, 1), (&good, 0)] {
        let revoked: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_revoked_dependencies
             WHERE kind = 'contribution' AND target = ?",
        )
        .bind(contribution.as_str())
        .fetch_one(db)
        .await
        .expect("revoked contribution");
        assert_eq!(revoked, expected);
    }
    let legacy_exclusions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM look_credential_exclusions")
            .fetch_one(db)
            .await
            .expect("legacy exclusions");
    assert_eq!(legacy_exclusions, 0);
    sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
        .bind(bad.as_str())
        .execute(db)
        .await
        .expect("automated contribution update");
    sqlx::query("UPDATE contribution_images SET status = 'accepted' WHERE contribution_id = ?")
        .bind(bad.as_str())
        .execute(db)
        .await
        .expect("automated image update");
    let bad_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(bad.as_str())
        .fetch_one(db)
        .await
        .expect("rejected status");
    assert_eq!(bad_status, "quarantined");
    let image_statuses: Vec<String> =
        sqlx::query_scalar("SELECT status FROM contribution_images WHERE contribution_id = ?")
            .bind(bad.as_str())
            .fetch_all(db)
            .await
            .expect("rejected image status");
    assert!(image_statuses.iter().all(|status| status == "quarantined"));
    let request: (i64, i64) = sqlx::query_as(
        "SELECT generation, reverted_version FROM look_refit_requests WHERE look = ?",
    )
    .bind(LOOK_1)
    .fetch_one(db)
    .await
    .expect("refit request");
    assert_eq!(request, (1, 3));

    let audit: String = sqlx::query_scalar(
        "SELECT payload FROM audit_log WHERE type = 'revert' ORDER BY seq DESC LIMIT 1",
    )
    .fetch_one(db)
    .await
    .expect("revert audit");
    let audit: serde_json::Value = serde_json::from_str(&audit).expect("audit payload");
    assert_eq!(audit["reason"], "contributor introduced a hue shift");
    assert_eq!(audit["plan"]["target_version"], 1);
    assert_eq!(
        audit["plan"]["rejected_contributions"][0].as_str(),
        Some(bad.as_str())
    );
    assert_eq!(
        audit["plan"]["restoration"]["historical_needs_rebuild"]["reason"],
        "dependency_history_unavailable"
    );

    let queued: Vec<(String, i64)> = sqlx::query_as(
        "SELECT kind, COUNT(*) FROM jobs
         WHERE kind IN ('publish_site', 'revert_refit') AND status = 'queued'
         GROUP BY kind ORDER BY kind",
    )
    .fetch_all(db)
    .await
    .expect("queued jobs");
    assert_eq!(
        queued,
        vec![
            ("publish_site".to_owned(), 1),
            ("revert_refit".to_owned(), 1),
        ]
    );
}

#[tokio::test]
async fn revert_validates_permission_reason_version_and_contributions_before_mutation() {
    use looklab_server::admin::ManifestArtifact;

    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let outsider = credential(db, "new", "[]", 0.5).await;
    let contributor = credential(db, "new", "[]", 0.5).await;
    let withdrawn = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    sqlx::query("UPDATE contributions SET status = 'withdrawn' WHERE id = ?")
        .bind(withdrawn.as_str())
        .execute(db)
        .await
        .expect("withdraw contribution");
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifact = ManifestArtifact {
        name: "look.looklab".to_owned(),
        sha256: "ab".repeat(32),
    };
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        std::slice::from_ref(&artifact),
        &[],
    )
    .await
    .expect("version 1");
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, 2, 'provisional', NULL, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(LOOK_1)
    .execute(db)
    .await
    .expect("provisional version");

    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        2,
        &[],
        "not published",
    )
    .await
    .expect_err("unpublished target");
    assert!(matches!(error, AdminError::VersionNotPublished));
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        9,
        &[],
        "missing",
    )
    .await
    .expect_err("unknown target");
    assert!(matches!(error, AdminError::UnknownVersion));
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&withdrawn),
        "withdrawn contribution",
    )
    .await
    .expect_err("withdrawn contribution");
    assert!(matches!(error, AdminError::ContributionWithdrawn));
    let unrelated = add_contribution(db, &contributor, "canon/canon-r5", "library", None).await;
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&unrelated),
        "unrelated contribution",
    )
    .await
    .expect_err("unrelated contribution");
    assert!(matches!(error, AdminError::ContributionNotInLookEvidence));
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&ContributionId::new()),
        "unknown contribution",
    )
    .await
    .expect_err("unknown contribution");
    assert!(matches!(error, AdminError::UnknownContribution));
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "   ",
    )
    .await
    .expect_err("empty reason");
    assert!(matches!(error, AdminError::InvalidReason));
    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&outsider),
        &look(LOOK_1),
        1,
        &[],
        "not a moderator",
    )
    .await
    .expect_err("insufficient role");
    assert!(matches!(error, AdminError::NotAuthorized));

    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
        .bind(LOOK_1)
        .fetch_one(db)
        .await
        .expect("versions");
    assert_eq!(versions, 2);
    let exclusions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM contribution_rejections")
        .fetch_one(db)
        .await
        .expect("rejections");
    assert_eq!(exclusions, 0);
    let audit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE type = 'revert'")
        .fetch_one(db)
        .await
        .expect("audit");
    assert_eq!(audit, 0);
    let refit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("refit job");
    assert_eq!(refit, 0);
    let requested: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_refit_requests")
        .fetch_one(db)
        .await
        .expect("refit request");
    assert_eq!(requested, 0);
}

#[tokio::test]
async fn a_published_native_version_without_an_accepted_marker_is_not_revertible() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    for version in [1, 2] {
        let artifacts = version_artifacts(
            &fixture.blobs,
            LOOK_1,
            version,
            version,
            serde_json::json!({}),
        );
        admin::write_version_manifest(
            db,
            &look(LOOK_1),
            VersionId::new(version),
            admin::VersionStatus::Published,
            &artifacts,
            &[],
        )
        .await
        .expect("version");
    }

    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        2,
        &[],
        "not an accepted release",
    )
    .await
    .expect_err("unselected published version");
    assert!(matches!(error, AdminError::VersionNotPublished));

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "accepted historical release",
    )
    .await
    .expect("accepted history remains revertible");
    assert_eq!(reverted.target_version, 1);
    assert_eq!(
        reverted.restoration,
        Restoration::HistoricalNeedsRebuild {
            reason: RestorationHold::DependencyHistoryUnavailable
        }
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ? AND version = ?")
            .bind(LOOK_1)
            .bind(i64::from(reverted.new_version))
            .fetch_one(db)
            .await
            .expect("restored status");
    assert_eq!(status, admin::VersionStatus::Deprecated.as_str());
    let target = PublishedTargetId {
        look: LookId::new(LOOK_1).expect("look"),
        version: TargetVersion::new(reverted.new_version).expect("version"),
    };
    let mut conn = db.acquire().await.expect("connection");
    let eligibility = model_store::target_eligibility(&mut conn, &target)
        .await
        .expect("eligibility")
        .expect("held eligibility");
    assert_eq!(eligibility.status, TargetEligibilityStatus::Invalidated);
    let revision: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_subject_revisions
         WHERE subject_key = ? AND revision = ? AND role = 'target'",
    )
    .bind(format!("shared_family:{LOOK_1}"))
    .bind(i64::from(reverted.new_version))
    .fetch_one(&mut *conn)
    .await
    .expect("subject revision");
    assert_eq!(revision, 1, "held revert must stay resolvable history");
}

#[tokio::test]
async fn lut_revert_promotes_only_authoritative_eligible_proof() {
    use looklab_schema::BasePin;

    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let evidence = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    let base = PublishedTargetId {
        look: LookId::new("classic-chrome").expect("base look"),
        version: TargetVersion::new(1).expect("base version"),
    };
    accept_family_version(db, &look("classic-chrome"), 1).await;
    let base_artifacts = version_artifacts(
        &fixture.blobs,
        "classic-chrome",
        1,
        1,
        serde_json::json!({}),
    );
    admin::write_version_manifest(
        db,
        &look("classic-chrome"),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &base_artifacts,
        &[],
    )
    .await
    .expect("base version");
    let mut conn = db.acquire().await.expect("connection");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: base.clone(),
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![
                DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(
                        &ContributionRef::new(evidence.as_str()).expect("contribution"),
                    ),
                ),
                DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
                ),
            ],
        },
        0,
    )
    .await
    .expect("base eligibility");

    let donor = look("donor-lut");
    let donor_artifacts = lut_version_artifacts(&fixture.blobs, "donor-lut", 1, 1);
    admin::write_version_manifest(
        db,
        &donor,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &donor_artifacts,
        &[],
    )
    .await
    .expect("donor version");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: LookId::new("donor-lut").expect("look"),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::LutComposition,
            artifacts: Vec::new(),
            base_pin: Some(BasePin::LookVersion(base.clone())),
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![DependencyRef::required(
                DependencyKind::PublishedTarget,
                DependencyTarget::published_target(&base),
            )],
        },
        0,
    )
    .await
    .expect("donor eligibility");
    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &donor,
        1,
        &[],
        "carry proof",
    )
    .await
    .expect("eligible revert");
    let new_target = PublishedTargetId {
        look: LookId::new("donor-lut").expect("look"),
        version: TargetVersion::new(reverted.new_version).expect("new version"),
    };
    let carried = model_store::target_eligibility(&mut conn, &new_target)
        .await
        .expect("eligibility")
        .expect("carried eligibility");
    assert_eq!(carried.status, TargetEligibilityStatus::Eligible);
    assert_eq!(carried.base_pin, Some(BasePin::LookVersion(base)));

    for name in ["legacy-lut", "invalid-lut"] {
        let slug = look(name);
        let artifacts = lut_version_artifacts(&fixture.blobs, name, 1, 1);
        admin::write_version_manifest(
            db,
            &slug,
            VersionId::new(1),
            admin::VersionStatus::Published,
            &artifacts,
            &[],
        )
        .await
        .expect("legacy version");
        sqlx::query(
            "INSERT INTO model_legacy_scopes
             (kind, target, family, applicability, reason, recorded_at)
             VALUES ('look_version', ?, ?, 'unassessed', 'epoch_and_sim_unrecorded', 0)",
        )
        .bind(format!("{name}@1"))
        .bind(name)
        .execute(&mut *conn)
        .await
        .expect("legacy marker");
        if name == "invalid-lut" {
            model_store::record_target_eligibility(
                &mut conn,
                &TargetEligibility {
                    id: EligibilitySnapshotId::from_digest([0; 32]),
                    target: PublishedTargetId {
                        look: LookId::new(name).expect("look"),
                        version: TargetVersion::new(1).expect("version"),
                    },
                    basis: TargetBasis::LutComposition,
                    artifacts: Vec::new(),
                    base_pin: None,
                    support_uncertainty_source: None,
                    status: TargetEligibilityStatus::Invalidated,
                    reasons: Vec::new(),
                    dependencies: Vec::new(),
                },
                0,
            )
            .await
            .expect("invalidated eligibility");
        }
        let reverted = admin::revert(
            db,
            &fixture.blobs,
            actor(&moderator),
            &slug,
            1,
            &[],
            "unassessed history",
        )
        .await
        .expect("legacy revert");
        let target = PublishedTargetId {
            look: LookId::new(name).expect("look"),
            version: TargetVersion::new(reverted.new_version).expect("version"),
        };
        let expected = if name == "invalid-lut" {
            RestorationHold::DependencyInvalidated
        } else {
            RestorationHold::DependencyHistoryUnavailable
        };
        assert_eq!(
            reverted.restoration,
            Restoration::HistoricalNeedsRebuild { reason: expected },
            "{name}"
        );
        let held = model_store::target_eligibility(&mut conn, &target)
            .await
            .expect("eligibility")
            .expect("held eligibility");
        assert_eq!(
            held.status,
            TargetEligibilityStatus::Invalidated,
            "{name} must not gain eligible authority"
        );
    }
}

#[tokio::test]
async fn revert_marks_pinned_lut_dependents_for_rebase() {
    use looklab_model::file::{BaseReference, BaseStatus, LookVersion, Slug};
    use looklab_server::admin::LutRebaseReason;

    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifact = version_artifacts(
        &fixture.blobs,
        LOOK_1,
        1,
        1,
        serde_json::json!({"probes": []}),
    );
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifact,
        &[],
    )
    .await
    .expect("base version");
    let dependent = look("donor-lut");
    let dependent_artifacts = version_artifacts(
        &fixture.blobs,
        "donor-lut",
        1,
        1,
        serde_json::json!({"probes": []}),
    );
    admin::record_lut_base(
        db,
        &dependent,
        1,
        &BaseReference::Look {
            slug: Slug::new(LOOK_1).expect("slug"),
            version: LookVersion::new("1.0.0").expect("version"),
        },
        &BaseStatus::Pinned,
    )
    .await
    .expect("base pin");
    admin::write_version_manifest(
        db,
        &dependent,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &dependent_artifacts,
        &[],
    )
    .await
    .expect("dependent version");
    sqlx::query("DELETE FROM jobs")
        .execute(db)
        .await
        .expect("clear jobs");
    assert!(
        admin::pending_lut_rebases(db)
            .await
            .expect("pending")
            .is_empty()
    );

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "restore the fit",
    )
    .await
    .expect("revert");

    assert!(
        reverted
            .lut_dependents
            .iter()
            .any(|version| version.look == dependent && version.version == 1)
    );
    let pending = admin::pending_lut_rebases(db).await.expect("pending");
    assert!(pending.iter().any(|work| {
        work.look == dependent && work.version == 1 && work.reason == LutRebaseReason::BasePublished
    }));
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'lut_rebase' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("lut rebase job");
    assert_eq!(queued, 1);
}

#[tokio::test]
async fn hiding_removing_and_relisting_a_look_records_each_state() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let look_slug = "superseded-lut";
    sqlx::query(
        "INSERT INTO look_versions (id, look, version, status, changelog, created_at)
         VALUES (?, ?, 1, 'published', NULL, 0)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(look_slug)
    .execute(db)
    .await
    .expect("look version");
    let look = LookSlug::try_from(look_slug.to_owned()).expect("look");

    let hidden = admin::hide_look(db, &moderator, &look, "superseded by a native Look")
        .await
        .expect("hide");
    assert_eq!(hidden.listing, admin::ListingState::Hidden);
    assert_eq!(hidden.superseded_by, None);
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'publish_site' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("publication job");
    assert_eq!(queued, 1);
    let error = admin::hide_look(db, &moderator, &look, "again")
        .await
        .expect_err("already hidden");
    assert!(matches!(error, AdminError::LookState(_)));

    let removed = admin::remove_look(db, &moderator, &look, "the LUT is retracted")
        .await
        .expect("remove");
    assert_eq!(removed.listing, admin::ListingState::Removed);

    let relisted = admin::relist_look(db, &moderator, &look, "moderator restored it")
        .await
        .expect("relist");
    assert_eq!(relisted.listing, admin::ListingState::Listed);
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'publish_site' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("publication job");
    assert_eq!(queued, 1);

    let audits: Vec<(String, i64)> = sqlx::query_as(
        "SELECT type, COUNT(*) FROM audit_log
         WHERE type IN ('look_hidden', 'look_removed', 'look_relisted')
         GROUP BY type ORDER BY type",
    )
    .fetch_all(db)
    .await
    .expect("audit rows");
    assert_eq!(
        audits,
        vec![
            ("look_hidden".to_owned(), 1),
            ("look_relisted".to_owned(), 1),
            ("look_removed".to_owned(), 1),
        ]
    );

    let missing = LookSlug::try_from("no-such-look".to_owned()).expect("look");
    let error = admin::hide_look(db, &moderator, &missing, "unknown")
        .await
        .expect_err("unknown Look");
    assert!(matches!(error, AdminError::UnknownLook));
}

#[tokio::test]
async fn reanchor_updates_scoped_subject_anchor_with_cas() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let look = look(LOOK_1);
    accept_family_version(db, &look, 1).await;
    let artifacts = version_artifacts(&fixture.blobs, LOOK_1, 1, 1, serde_json::json!({}));
    admin::write_version_manifest(
        db,
        &look,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version 1");
    let family = LookId::new(LOOK_1).expect("family");
    let subject = SubjectKey::for_family(&family);
    let mut conn = db.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        0,
    )
    .await
    .expect("subject");
    admin::reanchor(db, actor(&moderator), &look, 1, "new root session")
        .await
        .expect("reanchor");
    let state = model_store::subject_state(&mut conn, &subject)
        .await
        .expect("state")
        .expect("scoped state");
    assert_eq!(state.anchor_revision, Some(1));
    assert!(
        !model_store::set_subject_anchor(&mut conn, &subject, Some(0), 1, 1)
            .await
            .expect("stale CAS")
    );
}

#[tokio::test]
async fn revert_advances_existing_family_head_for_clean_history() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let evidence = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifacts = version_artifacts(&fixture.blobs, LOOK_1, 1, 1, serde_json::json!({}));
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version 1");
    let family = LookId::new(LOOK_1).expect("family");
    let mut conn = db.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        0,
    )
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_family_heads
         (family, head_version, source_limited, numerical, release_gates, report_id,
          assessed_generation, applicability, invalidated_reason, updated_at)
         VALUES (?, 1, 1, 'passed', 'passed', NULL, 0, 'current', NULL, 0)",
    )
    .bind(family.as_str())
    .execute(&mut *conn)
    .await
    .expect("head");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: family.clone(),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![
                DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(
                        &ContributionRef::new(evidence.as_str()).expect("contribution"),
                    ),
                ),
                DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
                ),
            ],
        },
        0,
    )
    .await
    .expect("source eligibility");
    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "restore the fit",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.restoration, Restoration::Current);
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("advanced head");
    assert_eq!(head.target.version.get(), reverted.new_version);
    assert!(head.report.is_none());
    let new_target = PublishedTargetId {
        look: family.clone(),
        version: TargetVersion::new(reverted.new_version).expect("version"),
    };
    let eligibility = model_store::target_eligibility(&mut conn, &new_target)
        .await
        .expect("eligibility")
        .expect("bound eligibility");
    assert_eq!(eligibility.status, TargetEligibilityStatus::Eligible);
    let bindings = model_store::artifacts(
        &mut conn,
        "family_target",
        &FamilyTargetId {
            family: family.clone(),
            version: new_target.version,
        }
        .to_key(),
    )
    .await
    .expect("bindings");
    assert!(
        bindings
            .iter()
            .any(|binding| binding.role == ArtifactRole::Manifest)
    );
    let standing: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM model_dependency_generations
         WHERE kind = 'standing' AND target = ?",
    )
    .bind(contributor.as_str())
    .fetch_one(&mut *conn)
    .await
    .expect("standing generation");
    assert_eq!(standing, 0, "rejection does not change standing");
}

#[tokio::test]
async fn revert_head_is_invalidated_with_the_restored_target_dependencies() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let evidence = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifacts = version_artifacts(&fixture.blobs, LOOK_1, 1, 1, serde_json::json!({}));
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version 1");
    let family = LookId::new(LOOK_1).expect("family");
    let mut conn = db.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        0,
    )
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_family_heads
         (family, head_version, source_limited, numerical, release_gates, report_id,
          assessed_generation, applicability, invalidated_reason, updated_at)
         VALUES (?, 1, 1, 'passed', 'passed', NULL, 0, 'current', NULL, 0)",
    )
    .bind(family.as_str())
    .execute(&mut *conn)
    .await
    .expect("head");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: family.clone(),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![
                DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(
                        &ContributionRef::new(evidence.as_str()).expect("contribution"),
                    ),
                ),
                DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
                ),
            ],
        },
        0,
    )
    .await
    .expect("source eligibility");

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "restore the prior fit",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.restoration, Restoration::Current);

    let source = PublishedTargetId {
        look: family.clone(),
        version: TargetVersion::new(1).expect("version"),
    };
    let affected = model_store::invalidate(
        &mut conn,
        &DependencyRef::required(
            DependencyKind::PublishedTarget,
            DependencyTarget::published_target(&source),
        ),
        InvalidationReason::EvidenceLoss,
        10,
    )
    .await
    .expect("invalidate restored source");
    assert!(affected.family_heads >= 1);
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("advanced head");
    assert!(matches!(
        head.applicability,
        Applicability::Invalidated { .. }
    ));
}

fn evidence_dependencies(contributions: &[&ContributionId]) -> Vec<DependencyRef> {
    let mut dependencies = contributions
        .iter()
        .map(|contribution| {
            DependencyRef::required(
                DependencyKind::Contribution,
                DependencyTarget::contribution(
                    &ContributionRef::new(contribution.as_str()).expect("contribution"),
                ),
            )
        })
        .collect::<Vec<_>>();
    dependencies.push(DependencyRef::required(
        DependencyKind::Image,
        DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
    ));
    dependencies
}

async fn native_target_with_evidence(
    fixture: &Fixture,
    family: &LookId,
    dependencies: Vec<DependencyRef>,
) {
    let db = &fixture.db;
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifacts = version_artifacts(&fixture.blobs, LOOK_1, 1, 1, serde_json::json!({}));
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version");
    let mut conn = db.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        0,
    )
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_family_heads
         (family, head_version, source_limited, numerical, release_gates, report_id,
          assessed_generation, applicability, invalidated_reason, updated_at)
         VALUES (?, 1, 1, 'passed', 'passed', NULL, 0, 'current', NULL, 0)",
    )
    .bind(family.as_str())
    .execute(&mut *conn)
    .await
    .expect("head");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: family.clone(),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies,
        },
        0,
    )
    .await
    .expect("eligibility");
}

#[tokio::test]
async fn rejecting_evidence_in_the_target_closure_holds_the_restored_copy() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let bad = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    let good = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    for contribution in [&bad, &good] {
        sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
            .bind(contribution.as_str())
            .execute(db)
            .await
            .expect("accept contribution");
    }
    let family = LookId::new(LOOK_1).expect("family");
    native_target_with_evidence(&fixture, &family, evidence_dependencies(&[&bad])).await;

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&bad),
        "bad evidence",
    )
    .await
    .expect("revert");
    assert_eq!(
        reverted.restoration,
        Restoration::HistoricalNeedsRebuild {
            reason: RestorationHold::DependencyInvalidated
        }
    );
    let status: String =
        sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ? AND version = ?")
            .bind(LOOK_1)
            .bind(i64::from(reverted.new_version))
            .fetch_one(db)
            .await
            .expect("restored status");
    assert_eq!(status, admin::VersionStatus::Deprecated.as_str());
    let mut conn = db.acquire().await.expect("connection");
    let new_target = PublishedTargetId {
        look: family.clone(),
        version: TargetVersion::new(reverted.new_version).expect("version"),
    };
    let eligibility = model_store::target_eligibility(&mut conn, &new_target)
        .await
        .expect("eligibility")
        .expect("held eligibility");
    assert_eq!(eligibility.status, TargetEligibilityStatus::Invalidated);
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.target.version.get(), 1);
    let refit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_refit_requests WHERE look = ?")
        .bind(LOOK_1)
        .fetch_one(db)
        .await
        .expect("refit request");
    assert_eq!(refit, 1);
    let job: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("refit job");
    assert_eq!(job, 1);
    let statuses: Vec<(String, String)> =
        sqlx::query_as("SELECT id, status FROM contributions WHERE id IN (?, ?)")
            .bind(bad.as_str())
            .bind(good.as_str())
            .fetch_all(db)
            .await
            .expect("statuses");
    assert!(
        statuses
            .iter()
            .any(|(id, status)| id == bad.as_str() && status == "quarantined")
    );
    assert!(
        statuses
            .iter()
            .any(|(id, status)| id == good.as_str() && status == "accepted")
    );
}

#[tokio::test]
async fn rejecting_evidence_outside_the_target_closure_keeps_the_restore_current() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let bad = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    let good = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    for contribution in [&bad, &good] {
        sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
            .bind(contribution.as_str())
            .execute(db)
            .await
            .expect("accept contribution");
    }
    let family = LookId::new(LOOK_1).expect("family");
    native_target_with_evidence(&fixture, &family, evidence_dependencies(&[&good])).await;

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        std::slice::from_ref(&bad),
        "unused bad evidence",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.restoration, Restoration::Current);
    let status: String =
        sqlx::query_scalar("SELECT status FROM look_versions WHERE look = ? AND version = ?")
            .bind(LOOK_1)
            .bind(i64::from(reverted.new_version))
            .fetch_one(db)
            .await
            .expect("restored status");
    assert_eq!(status, admin::VersionStatus::Published.as_str());
    let mut conn = db.acquire().await.expect("connection");
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("head");
    assert_eq!(head.target.version.get(), reverted.new_version);
    let new_target = PublishedTargetId {
        look: family.clone(),
        version: TargetVersion::new(reverted.new_version).expect("version"),
    };
    let eligibility = model_store::target_eligibility(&mut conn, &new_target)
        .await
        .expect("eligibility")
        .expect("carried eligibility");
    assert_eq!(eligibility.status, TargetEligibilityStatus::Eligible);
    let bad_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(bad.as_str())
        .fetch_one(db)
        .await
        .expect("bad status");
    assert_eq!(bad_status, "quarantined");
    let good_status: String = sqlx::query_scalar("SELECT status FROM contributions WHERE id = ?")
        .bind(good.as_str())
        .fetch_one(db)
        .await
        .expect("good status");
    assert_eq!(good_status, "accepted");
    let refit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_refit_requests WHERE look = ?")
        .bind(LOOK_1)
        .fetch_one(db)
        .await
        .expect("refit request");
    assert_eq!(refit, 1);
    let job: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("refit job");
    assert_eq!(job, 1);
}

#[tokio::test]
async fn rejecting_a_lut_base_contribution_holds_the_restored_lut_copy() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let bad = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = ?")
        .bind(bad.as_str())
        .execute(db)
        .await
        .expect("accept contribution");
    let base = PublishedTargetId {
        look: LookId::new("classic-chrome").expect("base look"),
        version: TargetVersion::new(1).expect("base version"),
    };
    accept_family_version(db, &look("classic-chrome"), 1).await;
    let base_artifacts = version_artifacts(
        &fixture.blobs,
        "classic-chrome",
        1,
        1,
        serde_json::json!({}),
    );
    admin::write_version_manifest(
        db,
        &look("classic-chrome"),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &base_artifacts,
        &[],
    )
    .await
    .expect("base version");
    let donor = look("donor-lut");
    let donor_artifacts = lut_version_artifacts(&fixture.blobs, "donor-lut", 1, 1);
    admin::write_version_manifest(
        db,
        &donor,
        VersionId::new(1),
        admin::VersionStatus::Published,
        &donor_artifacts,
        &[],
    )
    .await
    .expect("donor version");
    let mut conn = db.acquire().await.expect("connection");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: base.clone(),
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: evidence_dependencies(&[&bad]),
        },
        0,
    )
    .await
    .expect("base eligibility");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: LookId::new("donor-lut").expect("look"),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::LutComposition,
            artifacts: Vec::new(),
            base_pin: Some(looklab_schema::BasePin::LookVersion(base)),
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![DependencyRef::required(
                DependencyKind::PublishedTarget,
                DependencyTarget::published_target(&PublishedTargetId {
                    look: LookId::new("classic-chrome").expect("base look"),
                    version: TargetVersion::new(1).expect("base version"),
                }),
            )],
        },
        0,
    )
    .await
    .expect("donor eligibility");

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &donor,
        1,
        std::slice::from_ref(&bad),
        "bad base evidence",
    )
    .await
    .expect("revert");
    assert_eq!(
        reverted.restoration,
        Restoration::HistoricalNeedsRebuild {
            reason: RestorationHold::DependencyInvalidated
        }
    );
    let target = PublishedTargetId {
        look: LookId::new("donor-lut").expect("look"),
        version: TargetVersion::new(reverted.new_version).expect("version"),
    };
    let eligibility = model_store::target_eligibility(&mut conn, &target)
        .await
        .expect("eligibility")
        .expect("held eligibility");
    assert_eq!(eligibility.status, TargetEligibilityStatus::Invalidated);
    let refit: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_refit_requests WHERE look = ?")
        .bind("donor-lut")
        .fetch_one(db)
        .await
        .expect("refit request");
    assert_eq!(refit, 1);
    let job: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'revert_refit' AND status = 'queued'",
    )
    .fetch_one(db)
    .await
    .expect("refit job");
    assert_eq!(job, 1);
}

#[tokio::test]
async fn invalidated_accepted_versions_are_not_revertible_or_anchorable() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    let artifacts = version_artifacts(&fixture.blobs, LOOK_1, 1, 1, serde_json::json!({}));
    admin::write_version_manifest(
        db,
        &look(LOOK_1),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("version 1");
    let family = LookId::new(LOOK_1).expect("family");
    let mut conn = db.acquire().await.expect("connection");
    model_store::invalidate(
        &mut conn,
        &DependencyRef::required(
            DependencyKind::FamilyVersion,
            DependencyTarget::family_version(&FamilyTargetId {
                family: family.clone(),
                version: TargetVersion::new(1).expect("version"),
            }),
        ),
        InvalidationReason::Withdrawal,
        10,
    )
    .await
    .expect("revoke family version");

    let error = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "revoked history",
    )
    .await
    .expect_err("revoked version is not revertible");
    assert!(matches!(error, AdminError::VersionNotPublished));
    let error = admin::reanchor(db, actor(&moderator), &look(LOOK_1), 1, "revoked history")
        .await
        .expect_err("revoked version is not anchorable");
    assert!(matches!(error, AdminError::VersionNotPublished));
    let versions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_versions WHERE look = ?")
        .bind(LOOK_1)
        .fetch_one(db)
        .await
        .expect("versions");
    assert_eq!(versions, 1);
}

#[tokio::test]
async fn revert_restores_head_metadata_from_the_restored_target() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let moderator = credential(db, "new", "[\"moderator\"]", 0.5).await;
    let contributor = credential(db, "endorsed", "[]", 1.0).await;
    let evidence = add_contribution(db, &contributor, "fujifilm/x-t5", "library", None).await;
    accept_family_version(db, &look(LOOK_1), 1).await;
    for version in [1, 2] {
        let artifacts =
            version_artifacts(&fixture.blobs, LOOK_1, version, 1, serde_json::json!({}));
        admin::write_version_manifest(
            db,
            &look(LOOK_1),
            VersionId::new(version),
            admin::VersionStatus::Published,
            &artifacts,
            &[],
        )
        .await
        .expect("version");
    }
    accept_family_version(db, &look(LOOK_1), 2).await;
    let family = LookId::new(LOOK_1).expect("family");
    let subject = SubjectKey::for_family(&family);
    let mut conn = db.acquire().await.expect("connection");
    model_store::register_subject(
        &mut conn,
        &ModelSubject::SharedFamily {
            family: family.clone(),
        },
        0,
    )
    .await
    .expect("subject");
    sqlx::query(
        "INSERT INTO model_family_heads
         (family, head_version, source_limited, numerical, release_gates, report_id,
          assessed_generation, applicability, invalidated_reason, updated_at)
         VALUES (?, 2, 0, 'passed', 'passed', NULL, 7, 'current', NULL, 0)",
    )
    .bind(family.as_str())
    .execute(&mut *conn)
    .await
    .expect("head");
    sqlx::query(
        "INSERT INTO model_pending_work (subject_key, generation, updated_at) VALUES (?, 9, 0)",
    )
    .bind(subject.as_str())
    .execute(&mut *conn)
    .await
    .expect("pending generation");
    model_store::record_target_eligibility(
        &mut conn,
        &TargetEligibility {
            id: EligibilitySnapshotId::from_digest([0; 32]),
            target: PublishedTargetId {
                look: family.clone(),
                version: TargetVersion::new(1).expect("version"),
            },
            basis: TargetBasis::NativeFamily,
            artifacts: Vec::new(),
            base_pin: None,
            support_uncertainty_source: None,
            status: TargetEligibilityStatus::Eligible,
            reasons: Vec::new(),
            dependencies: vec![
                DependencyRef::required(
                    DependencyKind::Contribution,
                    DependencyTarget::contribution(
                        &ContributionRef::new(evidence.as_str()).expect("contribution"),
                    ),
                ),
                DependencyRef::required(
                    DependencyKind::Image,
                    DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
                ),
            ],
        },
        0,
    )
    .await
    .expect("source eligibility");

    let reverted = admin::revert(
        db,
        &fixture.blobs,
        actor(&moderator),
        &look(LOOK_1),
        1,
        &[],
        "restore the accepted fit",
    )
    .await
    .expect("revert");
    assert_eq!(reverted.restoration, Restoration::Current);
    let head = model_store::family_head(&mut conn, &family)
        .await
        .expect("head")
        .expect("advanced head");
    assert_eq!(head.target.version.get(), reverted.new_version);
    assert!(head.source_limited);
    assert_eq!(head.safety.numerical, SafetyCheck::Passed);
    assert_eq!(head.safety.release_gates, SafetyCheck::Passed);
    assert_eq!(head.report, None);
    let assessed: i64 =
        sqlx::query_scalar("SELECT assessed_generation FROM model_family_heads WHERE family = ?")
            .bind(family.as_str())
            .fetch_one(&mut *conn)
            .await
            .expect("assessed generation");
    assert_eq!(assessed, 9);
}

fn fence_sources(fitter: &str) -> NumericalSources {
    NumericalSources {
        fitter: SourceRef::new(fitter).expect("fitter"),
        evaluator: SourceRef::new("evaluator@1").expect("evaluator"),
        model: SourceRef::new("model@1").expect("model"),
    }
}

#[tokio::test]
async fn fenced_worker_proposal_checks_lease_sources_and_accepted_evidence() {
    let fixture = fixture().await;
    let sources = fence_sources("fit@1");
    model_store::sync_active_numerical_sources_fenced(&fixture.db, &sources, 0)
        .await
        .expect("sources");
    let job = JobId::new();
    sqlx::query(
        "INSERT INTO jobs (id, kind, payload, status, attempts, run_after, created_at)
         VALUES (?, 'refit', '{}', 'running', 1, 0, 0)",
    )
    .bind(job.as_str())
    .execute(&fixture.db)
    .await
    .expect("job");
    let body = BodyId::new("fujifilm/x-t5").expect("body");
    let dependency = DependencyRef::required(
        DependencyKind::Transform,
        DependencyTarget::transform(&body, 1),
    );
    let family = LookId::new("classic-chrome").expect("family");
    let target = FamilyTargetId {
        family,
        version: TargetVersion::new(1).expect("version"),
    };
    let family_dependency = DependencyRef::required(
        DependencyKind::FamilyVersion,
        DependencyTarget::family_version(&target),
    );
    let dependencies = [dependency.clone(), family_dependency.clone()];
    let mut conn = fixture.db.acquire().await.expect("connection");
    let snapshot = model_store::observe_dependency_snapshot(&mut conn, &dependencies, 0)
        .await
        .expect("snapshot");
    drop(conn);

    let proposal = |split_at: &str| WorkerProposal {
        kind: ProposalKind::EpochSplit,
        payload: format!(
            "body: fujifilm/x-t5\nfrom_epoch: 1\nnew_epoch: 2\nsplit_at: {split_at}\n"
        ),
        evidence: serde_json::json!({"source": "worker"}),
    };
    let lease = JobLease {
        job: &job,
        attempts: 1,
    };
    assert!(
        admin::submit_worker_proposal_fenced(
            &fixture.db,
            proposal("1.60"),
            lease,
            &sources,
            &dependencies,
            &snapshot,
        )
        .await
        .expect("valid proposal")
        .is_some()
    );

    assert!(matches!(
        admin::submit_worker_proposal_fenced(
            &fixture.db,
            proposal("1.61"),
            JobLease {
                job: &job,
                attempts: 2,
            },
            &sources,
            &dependencies,
            &snapshot,
        )
        .await,
        Err(AdminError::Conflict)
    ));
    assert!(matches!(
        admin::submit_worker_proposal_fenced(
            &fixture.db,
            proposal("1.62"),
            lease,
            &fence_sources("fit@2"),
            &dependencies,
            &snapshot,
        )
        .await,
        Err(AdminError::Conflict)
    ));

    let mut conn = fixture.db.acquire().await.expect("connection");
    let appended = DependencyRef::required(
        DependencyKind::Image,
        DependencyTarget::image(&ImageRef::new("image-1").expect("image")),
    );
    model_store::observe_dependency_snapshot(&mut conn, &[appended], 2)
        .await
        .expect("append");
    drop(conn);
    assert!(
        admin::submit_worker_proposal_fenced(
            &fixture.db,
            proposal("1.63"),
            lease,
            &sources,
            &dependencies,
            &snapshot,
        )
        .await
        .expect("append-only successor")
        .is_some()
    );

    let mut conn = fixture.db.acquire().await.expect("connection");
    model_store::invalidate(
        &mut conn,
        &family_dependency,
        InvalidationReason::EvidenceLoss,
        3,
    )
    .await
    .expect("invalidate");
    drop(conn);
    assert!(matches!(
        admin::submit_worker_proposal_fenced(
            &fixture.db,
            proposal("1.64"),
            lease,
            &sources,
            &dependencies,
            &snapshot,
        )
        .await,
        Err(AdminError::Conflict)
    ));
}

fn operator_cube() -> CubeFile {
    let size = 3;
    let mut data = Vec::new();
    for b in 0..size {
        for g in 0..size {
            for r in 0..size {
                let value = |n: u32| n as f32 / (size - 1) as f32;
                data.push([value(r), value(g), value(b)]);
            }
        }
    }
    CubeFile {
        lut: Lut3d {
            size,
            data,
            shaper: None,
        },
        domain: [0.0, 1.0],
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn operator_proposal(name: &str, bytes: &[u8], digest: &str) -> LutLookProposal {
    LutLookProposal {
        look: LookId::new(name).expect("look"),
        name: name.to_owned(),
        license: LicenseDeclaration {
            own_work: false,
            source_license: Some("CC0-1.0".to_owned()),
            source_url: None,
        },
        attribution: "a donor".to_owned(),
        lut: LutFile {
            file: "donated.cube".to_owned(),
            sha256: Sha256Hex::parse(digest).expect("digest"),
            size: bytes.len() as u64,
        },
        encoding: LutEncoding::Display {
            base: LutBase::BodyRealization {
                body: BodyId::new("fujifilm/x-e3").expect("body"),
                renderer: LutRenderer::XRawStudio,
                film_simulation: None,
                dynamic_range: Some(LutDynamicRange::Dr200),
                notes: "X-E3 RAW; film simulation not yet known.".to_owned(),
            },
        },
        emulates: None,
    }
}

async fn assert_operator_admission_wrote_nothing(fixture: &Fixture) {
    for table in [
        "proposals",
        "moderation_decisions",
        "jobs",
        "audit_log",
        "lut_uploads",
    ] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&fixture.db)
            .await
            .expect("count");
        assert_eq!(count, 0, "{table} has rows");
    }
    assert_eq!(std::fs::read_dir(&fixture.blobs).expect("blobs").count(), 0);
}

#[tokio::test]
async fn an_operator_lut_admission_stores_an_approved_proposal_and_job() {
    let fixture = fixture().await;
    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let digest = digest_hex(bytes);
    let proposal = operator_proposal("operator-lut", bytes, &digest);
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let admitted = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal: proposal.clone(),
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect("admit");
    assert_eq!(admitted.look.as_str(), "operator-lut");

    let (status, credential, payload): (String, Option<String>, String) =
        sqlx::query_as("SELECT status, credential_id, payload FROM proposals WHERE id = ?")
            .bind(admitted.proposal.as_str())
            .fetch_one(&fixture.db)
            .await
            .expect("proposal");
    assert_eq!(status, "approved");
    assert!(credential.is_none());
    let stored =
        looklab_schema::validate_proposal(ProposalKind::LutLook, &payload).expect("stored payload");
    let ValidatedPayload::LutLook(stored) = stored else {
        panic!("not a LUT proposal");
    };
    assert_eq!(stored, proposal);

    let (moderator, decision, reason): (String, String, String) = sqlx::query_as(
        "SELECT moderator_credential, decision, reason FROM moderation_decisions
         WHERE proposal_id = ?",
    )
    .bind(admitted.proposal.as_str())
    .fetch_one(&fixture.db)
    .await
    .expect("decision");
    assert_eq!(moderator, "cli");
    assert_eq!(decision, "approve");
    assert_eq!(reason, "donation round");

    let (kind, job): (String, String) =
        sqlx::query_as("SELECT kind, payload FROM jobs WHERE kind = 'lut_import'")
            .fetch_one(&fixture.db)
            .await
            .expect("job");
    assert_eq!(kind, "lut_import");
    let job: serde_json::Value = serde_json::from_str(&job).expect("job payload");
    assert_eq!(job["proposal_id"], admitted.proposal.as_str());

    let audit: String =
        sqlx::query_scalar("SELECT payload FROM audit_log WHERE type = 'moderation_decision'")
            .fetch_one(&fixture.db)
            .await
            .expect("audit");
    let audit: serde_json::Value = serde_json::from_str(&audit).expect("audit payload");
    assert_eq!(audit["actor"], "cli");
    assert_eq!(audit["decision"], "approve");
    assert_eq!(audit["proposal"], admitted.proposal.as_str());

    let (sha, size, uploader): (Vec<u8>, i64, Option<String>) =
        sqlx::query_as("SELECT sha256, size, credential_id FROM lut_uploads WHERE proposal_id = ?")
            .bind(admitted.proposal.as_str())
            .fetch_one(&fixture.db)
            .await
            .expect("upload");
    assert_eq!(sha, Sha256::digest(bytes).to_vec());
    assert_eq!(size, i64::try_from(bytes.len()).expect("size"));
    assert!(uploader.is_none());
    assert!(fixture.blobs.join(&digest).exists());
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_a_digest_mismatch() {
    let fixture = fixture().await;
    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let proposal = operator_proposal("operator-lut", bytes, &digest_hex(b"other bytes"));
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect_err("digest mismatch");
    assert!(matches!(
        error,
        ProposalError::InvalidFixture(message) if message.contains("digest")
    ));
    assert_operator_admission_wrote_nothing(&fixture).await;
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_an_over_limit_cube() {
    let fixture = fixture().await;
    let bytes = vec![0u8; usize::try_from(looklab_schema::MAX_LUT_BYTES).expect("limit") + 1];
    let proposal = operator_proposal("operator-lut", &bytes, &digest_hex(&bytes));
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes,
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect_err("over limit");
    assert!(matches!(
        error,
        ProposalError::InvalidPayload(message) if message.contains("size")
    ));
    assert_operator_admission_wrote_nothing(&fixture).await;
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_a_missing_reason() {
    let fixture = fixture().await;
    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let proposal = operator_proposal("operator-lut", bytes, &digest_hex(bytes));
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "  ".to_owned(),
        },
    )
    .await
    .expect_err("missing reason");
    assert!(matches!(
        error,
        ProposalError::Admin(AdminError::InvalidReason)
    ));
    assert_operator_admission_wrote_nothing(&fixture).await;
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_an_unpublished_base_look() {
    let fixture = fixture().await;
    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let mut proposal = operator_proposal("operator-lut", bytes, &digest_hex(bytes));
    proposal.encoding = LutEncoding::Display {
        base: LutBase::Look {
            look: LookId::new("missing-base").expect("look"),
            version: looklab_schema::LutLookVersion::new("1.0.0").expect("version"),
        },
    };
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect_err("unpublished base");
    assert!(matches!(
        error,
        ProposalError::InvalidPayload(message) if message.contains("not published")
    ));
    assert_operator_admission_wrote_nothing(&fixture).await;
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_a_zero_support_base_look() {
    let fixture = fixture().await;
    let db = &fixture.db;
    accept_family_version(db, &look("zero-base"), 1).await;
    let artifacts = zero_support_version_artifacts(&fixture.blobs, "zero-base", 1, 1);
    admin::write_version_manifest(
        db,
        &look("zero-base"),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("base version");

    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let mut proposal = operator_proposal("operator-lut", bytes, &digest_hex(bytes));
    proposal.encoding = LutEncoding::Display {
        base: LutBase::Look {
            look: LookId::new("zero-base").expect("look"),
            version: looklab_schema::LutLookVersion::new("1.0.0").expect("version"),
        },
    };
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect_err("zero-support base");
    assert!(matches!(
        error,
        ProposalError::InvalidPayload(message) if message.contains("not a usable rendering")
    ));
    let proposals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
        .fetch_one(&fixture.db)
        .await
        .expect("proposals");
    assert_eq!(proposals, 0);
}

#[tokio::test]
async fn an_operator_lut_admission_admits_an_unpinned_display_base_look() {
    let fixture = fixture().await;
    let db = &fixture.db;
    accept_family_version(db, &look("unpinned-base"), 1).await;
    let artifacts = unpinned_display_version_artifacts(&fixture.blobs, "unpinned-base", 1, 1);
    admin::write_version_manifest(
        db,
        &look("unpinned-base"),
        VersionId::new(1),
        admin::VersionStatus::Published,
        &artifacts,
        &[],
    )
    .await
    .expect("base version");

    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let mut proposal = operator_proposal("operator-lut", bytes, &digest_hex(bytes));
    proposal.encoding = LutEncoding::Display {
        base: LutBase::Look {
            look: LookId::new("unpinned-base").expect("look"),
            version: looklab_schema::LutLookVersion::new("1.0.0").expect("version"),
        },
    };
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect("an unpinned display base stays admissible");
}

#[tokio::test]
async fn an_operator_lut_admission_refuses_a_link_with_an_open_proposal() {
    let fixture = fixture().await;
    let cube = operator_cube();
    let text = write_cube(&cube.lut, "Donated", cube.domain);
    let bytes = text.as_bytes();
    let proposal = operator_proposal("operator-lut", bytes, &digest_hex(bytes));
    let payload = looklab_schema::encode_proposal(&proposal).expect("payload");
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES ('open-proposal', NULL, 'lut_look', ?, 'x', '[]', 'pending', 0)",
    )
    .bind(&payload)
    .execute(&fixture.db)
    .await
    .expect("open proposal");
    let store = FsBlobStore::new(&fixture.blobs).expect("store");
    let error = admit_lut(
        &fixture.db,
        &store,
        LutAdmission {
            proposal,
            cube: bytes.to_vec(),
            reason: "donation round".to_owned(),
        },
    )
    .await
    .expect_err("link conflict");
    assert!(matches!(
        error,
        ProposalError::InvalidPayload(message) if message.contains("open proposal")
    ));
    let proposals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM proposals")
        .fetch_one(&fixture.db)
        .await
        .expect("proposals");
    assert_eq!(proposals, 1);
    for table in ["moderation_decisions", "jobs", "audit_log", "lut_uploads"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
            .fetch_one(&fixture.db)
            .await
            .expect("count");
        assert_eq!(count, 0, "{table} has rows");
    }
    assert_eq!(std::fs::read_dir(&fixture.blobs).expect("blobs").count(), 0);
    let staged: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_staging WHERE expires_at = 0")
        .fetch_one(&fixture.db)
        .await
        .expect("staging");
    assert_eq!(staged, 1);
}
