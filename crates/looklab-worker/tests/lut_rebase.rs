// SPDX-License-Identifier: AGPL-3.0-or-later

#[cfg(feature = "integration")]
use looklab_export::{CubeFile, Lut3d, LutLookOptions, import_lut_look};
use looklab_model::file::{BaseReference, BaseStatus, LookVersion, Slug};
#[cfg(feature = "integration")]
use looklab_model::file::{
    CameraRendering, DeclaredEncoding, DynamicRange, LicenseRecord, LutDefinition, OutputEncoding,
};
#[cfg(feature = "integration")]
use looklab_model::{
    Look, LookDocument, LookIdentity, LookKind, ProbeCovariance, ProbeScalars, UncertaintyData,
    WbPolicy,
};
#[cfg(feature = "integration")]
use looklab_server::admin::ManifestArtifact;
use looklab_server::admin::{self, AdminError};
#[cfg(feature = "integration")]
use looklab_server::{CredentialId, RootAdmissionId};
#[cfg(feature = "integration")]
use looklab_trust::{Actor, Scope, ScopeRule, VersionId};
use looklab_upload::LookSlug;
#[cfg(feature = "integration")]
use looklab_worker::jobs::{JobId, JobKind, JobOwnership};
use looklab_worker::{Worker, WorkerConfig};
#[cfg(feature = "integration")]
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[cfg(feature = "integration")]
const LUT: &str = "donor-lut";
#[cfg(feature = "integration")]
const BASE: &str = "classic-chrome";
#[cfg(feature = "integration")]
const RENDERING: &str = "Classic Chrome";

struct Fixture {
    _dir: TempDir,
    worker: Worker,
}

async fn fixture() -> Fixture {
    let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let dir = TempDir::new().expect("temp dir");
    let mut config =
        WorkerConfig::new(dir.path().join("looklab.sqlite3"), dir.path().join("blobs"));
    config.data = data;
    config.site = dir.path().join("site");
    config.transforms = dir.path().join("transforms");
    std::fs::create_dir_all(&config.transforms).expect("transform dir");
    let worker = Worker::open(config).await.expect("worker");
    Fixture { _dir: dir, worker }
}

#[cfg(feature = "integration")]
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn look_slug(text: &str) -> LookSlug {
    LookSlug::try_from(text.to_owned()).expect("slug")
}

fn model_slug(text: &str) -> Slug {
    Slug::new(text).expect("slug")
}

#[cfg(feature = "integration")]
fn identity_cube() -> CubeFile {
    let size = 2;
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

#[cfg(feature = "integration")]
fn family(rendering: &str, dynamic_range: Option<DynamicRange>) -> BaseReference {
    BaseReference::FamilyRendering {
        maker: "Fujifilm".to_owned(),
        rendering: rendering.to_owned(),
        dynamic_range,
        notes: String::new(),
    }
}

#[cfg(feature = "integration")]
fn lut_document(
    slug: &str,
    version: &str,
    base: BaseReference,
    status: BaseStatus,
    base_document: Option<&LookDocument>,
    seed: u64,
) -> LookDocument {
    let cube = identity_cube();
    let values = cube
        .lut
        .data
        .iter()
        .flat_map(|rgb| rgb.iter().map(|value| f64::from(*value)))
        .collect();
    let lut = looklab_model::LutData::new(2, [0.0; 3], [1.0; 3], values).expect("lut");
    let definition = LutDefinition {
        name: "Donor".to_owned(),
        license: LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "donor".to_owned(),
        },
        encoding: DeclaredEncoding::DisplayReferred { base, status },
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let identity = LookIdentity::new(slug, version, LookKind::LutDefined, WbPolicy::Adaptive)
        .expect("identity");
    import_lut_look(
        identity,
        definition,
        &cube,
        base_document,
        LutLookOptions {
            seed,
            ..LutLookOptions::default()
        },
    )
    .expect("import")
    .document
}

#[cfg(feature = "integration")]
fn pending_family_lut() -> LookDocument {
    let values = vec![0.5; 24];
    let lut = looklab_model::LutData::new(2, [0.0; 3], [1.0; 3], values).expect("lut");
    let definition = LutDefinition {
        name: "Donor".to_owned(),
        license: LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "donor".to_owned(),
        },
        encoding: DeclaredEncoding::DisplayReferred {
            base: family(RENDERING, None),
            status: BaseStatus::BaseIdentifiedUnmeasured,
        },
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let identity = LookIdentity::new(LUT, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive)
        .expect("identity");
    LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut)
        .expect("document")
}

#[cfg(feature = "integration")]
/// A pending family LUT fitted against a measured base.
fn oracle_family_lut(
    slug: &str,
    base_document: &LookDocument,
    family_base: BaseReference,
    seed: u64,
) -> LookDocument {
    let pinned = BaseReference::Look {
        slug: base_document.file.look.slug.clone(),
        version: base_document.file.look.version.clone(),
    };
    let mut document = lut_document(
        slug,
        "1.0.0",
        pinned,
        BaseStatus::Pinned,
        Some(base_document),
        seed,
    );
    let definition = document.file.definition.as_mut().expect("lut definition");
    definition.encoding = DeclaredEncoding::DisplayReferred {
        base: family_base.clone(),
        status: BaseStatus::BaseIdentifiedUnmeasured,
    };
    document.file.provenance.base = Some(family_base);
    document.validate().expect("document");
    document
}

#[cfg(feature = "integration")]
fn native_document(
    slug: &str,
    version: &str,
    clip_k: f64,
    maker: &str,
    rendering: &str,
    volume_shift: Option<f64>,
) -> LookDocument {
    let identity = LookIdentity::new(slug, version, LookKind::NativeFamily, WbPolicy::Adaptive)
        .expect("identity");
    let mut look = Look::colorimetric();
    look.clip.k = clip_k;
    if let Some(shift) = volume_shift {
        look.color.g.volume.fill(shift);
    }
    let mut document = LookDocument::from_look(identity, look).expect("document");
    document.file.rendering = Some(CameraRendering {
        maker: maker.to_owned(),
        name: rendering.to_owned(),
    });
    let count = looklab_model::probes::v1().probes.len();
    let mut covariance = Vec::with_capacity(count * 9);
    for _ in 0..count {
        covariance.extend_from_slice(&[0.001, 0.0, 0.0, 0.0, 0.001, 0.0, 0.0, 0.0, 0.001]);
    }
    document
        .with_uncertainty(UncertaintyData {
            probe_cov: Some(ProbeCovariance::new(count, covariance.clone()).expect("covariance")),
            probe_cov_post: Some(ProbeCovariance::new(count, covariance).expect("covariance")),
            support: Some(ProbeScalars::new(vec![1.0; count]).expect("support")),
            probe_pre_reliable: Some(ProbeScalars::new(vec![1.0; count]).expect("identified")),
            ..UncertaintyData::default()
        })
        .expect("uncertainty")
}

#[cfg(feature = "integration")]
fn native_at(
    version: &str,
    clip_k: f64,
    maker: &str,
    rendering: &str,
    volume_shift: Option<f64>,
) -> LookDocument {
    native_document(BASE, version, clip_k, maker, rendering, volume_shift)
}

#[cfg(feature = "integration")]
fn native(version: &str, clip_k: f64) -> LookDocument {
    native_at(version, clip_k, "Fujifilm", RENDERING, None)
}

#[cfg(feature = "integration")]
async fn publish(worker: &Worker, slug: &str, version: u32, document: &LookDocument) {
    let look = look_slug(slug);
    looklab_worker::lut_rebase::record_lut_pin(worker.pool(), &look, version, document)
        .await
        .expect("pin");
    let bytes = document.encode().expect("encode");
    let sha256 = hex(&Sha256::digest(&bytes));
    std::fs::write(worker.config().blobs.join(&sha256), &bytes).expect("blob");
    admin::write_version_manifest(
        worker.pool(),
        &look,
        VersionId::new(version),
        &[ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256,
        }],
        &[],
    )
    .await
    .expect("manifest");
}

#[cfg(feature = "integration")]
async fn artifact(worker: &Worker, slug: &str, version: u32, name: &str) -> Vec<u8> {
    let artifacts = looklab_worker::db::manifest_artifacts(worker.pool(), slug, version)
        .await
        .expect("artifacts")
        .expect("manifest");
    let artifact = artifacts
        .iter()
        .find(|artifact| artifact.name == name)
        .expect("artifact");
    std::fs::read(worker.config().blobs.join(&artifact.sha256)).expect("blob")
}

#[cfg(feature = "integration")]
async fn artifact_bytes(worker: &Worker, slug: &str, version: u32) -> Vec<u8> {
    artifact(worker, slug, version, "look.looklab").await
}

#[cfg(feature = "integration")]
async fn published(worker: &Worker, slug: &str, version: u32) -> LookDocument {
    LookDocument::decode(&artifact_bytes(worker, slug, version).await).expect("document")
}

#[cfg(feature = "integration")]
fn broken_lut_document(slug: &str) -> LookDocument {
    let values = vec![0.5; 24];
    let lut =
        looklab_model::LutData::new(2, [0.0, 0.1, 0.2], [1.0, 1.1, 1.2], values).expect("lut");
    let definition = LutDefinition {
        name: "Broken".to_owned(),
        license: LicenseRecord {
            own_work: true,
            source_license: None,
            source_url: None,
            attribution: "donor".to_owned(),
        },
        encoding: DeclaredEncoding::DisplayReferred {
            base: family(RENDERING, None),
            status: BaseStatus::BaseIdentifiedUnmeasured,
        },
        output: OutputEncoding::Srgb,
        lut: lut.record(),
        emulates: None,
        superseded_by: None,
        notes: Vec::new(),
    };
    let identity = LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive)
        .expect("identity");
    LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut)
        .expect("document")
}

#[cfg(feature = "integration")]
async fn drain(worker: &Worker) {
    loop {
        sqlx::query("DELETE FROM jobs WHERE kind = 'publish_site' AND status = 'queued'")
            .execute(worker.pool())
            .await
            .expect("clear publish");
        match worker.run_once().await {
            Ok(Some(_)) => {}
            Ok(None) => return,
            Err(error) => panic!("job failed: {error}"),
        }
    }
}

#[cfg(feature = "integration")]
async fn drain_fails(worker: &Worker) -> bool {
    loop {
        sqlx::query("DELETE FROM jobs WHERE kind = 'publish_site' AND status = 'queued'")
            .execute(worker.pool())
            .await
            .expect("clear publish");
        match worker.run_once().await {
            Ok(Some(_)) => {}
            Ok(None) => return false,
            Err(_) => return true,
        }
    }
}

#[cfg(feature = "integration")]
async fn established() -> Fixture {
    let fixture = fixture().await;
    let base = native("1.0.0", 0.005);
    publish(&fixture.worker, BASE, 1, &base).await;
    publish(
        &fixture.worker,
        LUT,
        1,
        &oracle_family_lut(
            LUT,
            &base,
            family(RENDERING, None),
            fixture.worker.config().seed,
        ),
    )
    .await;
    let summary = admin::pending_lut_rebases(fixture.worker.pool())
        .await
        .expect("pending");
    assert!(
        summary
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 1),
        "no pending work after publishing the source and base"
    );
    drain(&fixture.worker).await;
    fixture
}

#[cfg(feature = "integration")]
async fn insert_root(worker: &Worker, look: &str, version: u32) -> RootAdmissionId {
    let root = RootAdmissionId::new();
    let credential = CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', 'seeder', '[]', 1.2, 0, 'active')",
    )
    .bind(credential.as_str())
    .bind(credential.as_str().as_bytes())
    .execute(worker.pool())
    .await
    .expect("credential");
    let scope = admin::scope_json(&Scope::new([ScopeRule::new(None, Some(look_slug(look)))]))
        .expect("scope");
    sqlx::query(
        "INSERT INTO root_admissions
         (id, admitted_by, contribution_id, scope, reason, admitted_at)
         VALUES (?, ?, ?, ?, 'fixture', 0)",
    )
    .bind(root.as_str())
    .bind(credential.as_str())
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(scope)
    .execute(worker.pool())
    .await
    .expect("root admission");
    let version_id: (String,) =
        sqlx::query_as("SELECT id FROM look_versions WHERE look = ? AND version = ?")
            .bind(look)
            .bind(i64::from(version))
            .fetch_one(worker.pool())
            .await
            .expect("base version");
    sqlx::query(
        "INSERT INTO version_depends_on_root (look_version_id, root_admission_id) VALUES (?, ?)",
    )
    .bind(&version_id.0)
    .bind(root.as_str())
    .execute(worker.pool())
    .await
    .expect("dependency");
    root
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn publishing_the_family_base_resolves_the_pending_pin() {
    let fixture = established().await;
    let document = published(&fixture.worker, LUT, 2).await;
    assert!(document.is_matchable());
    assert_eq!(
        document.base(),
        Some(&BaseReference::Look {
            slug: model_slug(BASE),
            version: LookVersion::new("1.0.0").expect("version"),
        })
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_family_base_with_settings_stays_pending() {
    let fixture = fixture().await;
    let base = native("1.0.0", 0.005);
    let document = oracle_family_lut(
        LUT,
        &base,
        family(RENDERING, Some(DynamicRange::Dr400)),
        fixture.worker.config().seed,
    );
    publish(&fixture.worker, LUT, 1, &document).await;
    publish(&fixture.worker, BASE, 1, &base).await;
    drain(&fixture.worker).await;

    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 2)
            .await
            .expect("manifest")
            .is_none()
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .is_empty()
    );
    let pin = admin::lut_base_pin(fixture.worker.pool(), &look_slug(LUT), 1)
        .await
        .expect("pin")
        .expect("pin row");
    assert!(matches!(
        pin.base,
        BaseReference::FamilyRendering {
            dynamic_range: Some(DynamicRange::Dr400),
            ..
        }
    ));
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn normalized_rendering_labels_resolve_the_family_base() {
    let fixture = fixture().await;
    let base = native_at("1.0.0", 0.005, "fujifilm", "classic-chrome", None);
    let document = oracle_family_lut(
        LUT,
        &base,
        BaseReference::FamilyRendering {
            maker: "FUJIFILM".to_owned(),
            rendering: "Classic Chrome".to_owned(),
            dynamic_range: None,
            notes: String::new(),
        },
        fixture.worker.config().seed,
    );
    publish(&fixture.worker, LUT, 1, &document).await;
    publish(&fixture.worker, BASE, 1, &base).await;
    drain(&fixture.worker).await;

    let document = published(&fixture.worker, LUT, 2).await;
    assert_eq!(
        document.base(),
        Some(&BaseReference::Look {
            slug: model_slug(BASE),
            version: LookVersion::new("1.0.0").expect("version"),
        })
    );
}

#[tokio::test]
async fn self_and_cycle_closing_pins_are_rejected() {
    let fixture = fixture().await;
    let a = look_slug("look-a");
    let b = look_slug("look-b");
    let pinned = |slug: &str, version: &str| BaseReference::Look {
        slug: model_slug(slug),
        version: LookVersion::new(version).expect("version"),
    };

    assert!(matches!(
        admin::record_lut_base(
            fixture.worker.pool(),
            &a,
            2,
            &pinned("look-a", "1.0.0"),
            &BaseStatus::Pinned,
        )
        .await,
        Err(AdminError::CyclicBaseDependency)
    ));
    admin::record_lut_base(
        fixture.worker.pool(),
        &a,
        1,
        &BaseReference::neutral_baseline(),
        &BaseStatus::BaseIdentifiedUnmeasured,
    )
    .await
    .expect("a pin");
    admin::record_lut_base(
        fixture.worker.pool(),
        &b,
        1,
        &pinned("look-a", "1.0.0"),
        &BaseStatus::Pinned,
    )
    .await
    .expect("b pin");
    assert!(matches!(
        admin::record_lut_base(
            fixture.worker.pool(),
            &a,
            2,
            &pinned("look-b", "1.0.0"),
            &BaseStatus::Pinned,
        )
        .await,
        Err(AdminError::CyclicBaseDependency)
    ));
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_later_base_version_creates_a_candidate_without_touching_old_bytes() {
    let fixture = established().await;
    let old = artifact_bytes(&fixture.worker, LUT, 2).await;

    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    drain(&fixture.worker).await;

    let document = published(&fixture.worker, LUT, 3).await;
    assert_eq!(
        document.base(),
        Some(&BaseReference::Look {
            slug: model_slug(BASE),
            version: LookVersion::new("2.0.0").expect("version"),
        })
    );
    assert_eq!(artifact_bytes(&fixture.worker, LUT, 2).await, old);
    let release: serde_json::Value =
        serde_json::from_slice(&artifact(&fixture.worker, LUT, 3, "release.json").await)
            .expect("release");
    assert_eq!(release["anchor"], 1);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn an_explicit_re_anchor_selects_the_later_anchor() {
    let fixture = established().await;
    sqlx::query("INSERT INTO audit_log (type, payload, ts) VALUES ('re_anchor', ?, 0)")
        .bind(format!(r#"{{"look":"{LUT}","version":2}}"#))
        .execute(fixture.worker.pool())
        .await
        .expect("re-anchor");
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    drain(&fixture.worker).await;

    let release: serde_json::Value =
        serde_json::from_slice(&artifact(&fixture.worker, LUT, 3, "release.json").await)
            .expect("release");
    assert_eq!(release["anchor"], 2);
    assert!(published(&fixture.worker, LUT, 3).await.is_matchable());
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn revoking_the_base_root_marks_the_lut_dependent() {
    let fixture = established().await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    drain(&fixture.worker).await;
    let old = artifact_bytes(&fixture.worker, LUT, 3).await;

    let root = insert_root(&fixture.worker, BASE, 2).await;
    let revocation = admin::revoke_root(
        fixture.worker.pool(),
        Actor::Cli,
        &root,
        "base root revoked",
    )
    .await
    .expect("revocation");

    assert!(
        revocation
            .lut_dependents
            .iter()
            .any(|dependent| dependent.look.as_str() == LUT && dependent.version == 3)
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT
                && work.version == 3
                && work.reason == admin::LutRebaseReason::BaseRevoked)
    );
    let audit = admin::audit_log(fixture.worker.pool(), 0, 500)
        .await
        .expect("audit");
    let entry = audit
        .entries
        .iter()
        .find(|entry| entry.kind == "root_revoked")
        .expect("root revoked entry");
    assert!(
        entry.payload["lut_dependents"]
            .as_array()
            .is_some_and(|dependents| dependents
                .iter()
                .any(|dependent| dependent["look"] == LUT && dependent["version"] == 3))
    );
    assert_eq!(artifact_bytes(&fixture.worker, LUT, 3).await, old);
    assert!(published(&fixture.worker, LUT, 3).await.is_matchable());
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn an_already_pending_dependent_is_still_reported_by_revocation() {
    let fixture = established().await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    drain(&fixture.worker).await;
    publish(&fixture.worker, BASE, 3, &native("3.0.0", 0.005)).await;
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 3)
    );

    let root = insert_root(&fixture.worker, BASE, 2).await;
    let revocation = admin::revoke_root(
        fixture.worker.pool(),
        Actor::Cli,
        &root,
        "base root revoked",
    )
    .await
    .expect("revocation");

    assert!(
        revocation
            .lut_dependents
            .iter()
            .any(|dependent| dependent.look.as_str() == LUT && dependent.version == 3)
    );
    let audit = admin::audit_log(fixture.worker.pool(), 0, 500)
        .await
        .expect("audit");
    let entry = audit
        .entries
        .iter()
        .find(|entry| entry.kind == "root_revoked")
        .expect("root revoked entry");
    assert!(
        entry.payload["lut_dependents"]
            .as_array()
            .is_some_and(|dependents| dependents
                .iter()
                .any(|dependent| dependent["look"] == LUT && dependent["version"] == 3))
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn revoked_ancestry_keeps_dependents_blocked() {
    let fixture = established().await;
    let mid = "mid-lut";
    let leaf = "leaf-lut";
    let seed = fixture.worker.config().seed;
    let base_document = published(&fixture.worker, BASE, 1).await;
    publish(
        &fixture.worker,
        mid,
        1,
        &oracle_family_lut(mid, &base_document, family(RENDERING, None), seed),
    )
    .await;
    drain(&fixture.worker).await;
    let mid_document = published(&fixture.worker, mid, 2).await;
    publish(
        &fixture.worker,
        leaf,
        1,
        &lut_document(
            leaf,
            "1.0.0",
            BaseReference::Look {
                slug: model_slug(mid),
                version: LookVersion::new("2.0.0").expect("version"),
            },
            BaseStatus::Pinned,
            Some(&mid_document),
            seed,
        ),
    )
    .await;
    let leaf_bytes = artifact_bytes(&fixture.worker, leaf, 1).await;

    let root = insert_root(&fixture.worker, BASE, 1).await;
    admin::revoke_root(
        fixture.worker.pool(),
        Actor::Cli,
        &root,
        "base root revoked",
    )
    .await
    .expect("revocation");
    drain(&fixture.worker).await;

    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), mid, 3)
            .await
            .expect("manifest")
            .is_none()
    );
    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), leaf, 2)
            .await
            .expect("manifest")
            .is_none()
    );
    let pending = admin::pending_lut_rebases(fixture.worker.pool())
        .await
        .expect("pending");
    assert!(pending.iter().any(|work| work.look.as_str() == mid
        && work.version == 2
        && work.reason == admin::LutRebaseReason::BaseRevoked));
    assert!(pending.iter().any(|work| work.look.as_str() == leaf
        && work.version == 1
        && work.reason == admin::LutRebaseReason::BaseRevoked));
    assert_eq!(artifact_bytes(&fixture.worker, leaf, 1).await, leaf_bytes);
    assert!(published(&fixture.worker, leaf, 1).await.is_matchable());
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_stale_attempt_does_not_publish_work() {
    let fixture = established().await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 2)
    );
    let stale = JobOwnership {
        id: JobId::new(),
        attempts: 1,
    };
    assert!(
        looklab_worker::lut_rebase::run(fixture.worker.pool(), fixture.worker.config(), &stale)
            .await
            .is_err()
    );
    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 3)
            .await
            .expect("manifest")
            .is_none()
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 2)
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn retrying_completed_work_does_not_publish_another_version() {
    let fixture = established().await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    let claimed = looklab_worker::jobs::claim(
        fixture.worker.pool(),
        3_600,
        looklab_worker::clock::now_unix(),
    )
    .await
    .expect("claim")
    .expect("job");
    assert_eq!(claimed.kind, JobKind::LutRebase);
    let ownership = claimed.ownership();
    looklab_worker::lut_rebase::run(fixture.worker.pool(), fixture.worker.config(), &ownership)
        .await
        .expect("first run");
    looklab_worker::lut_rebase::run(fixture.worker.pool(), fixture.worker.config(), &ownership)
        .await
        .expect("second run");

    assert!(published(&fixture.worker, LUT, 3).await.is_matchable());
    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 4)
            .await
            .expect("manifest")
            .is_none()
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_tampered_source_document_is_rejected() {
    let fixture = established().await;
    let artifacts = looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 1)
        .await
        .expect("artifacts")
        .expect("manifest");
    let source = artifacts
        .iter()
        .find(|artifact| artifact.name == "look.looklab")
        .expect("look artifact");
    let mut replacement = pending_family_lut();
    replacement.file.provenance.created = "2026-11-05T00:00:01Z".to_owned();
    std::fs::write(
        fixture.worker.config().blobs.join(&source.sha256),
        replacement.encode().expect("encode"),
    )
    .expect("tamper");
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;

    assert!(drain_fails(&fixture.worker).await);
    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 3)
            .await
            .expect("manifest")
            .is_none()
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_committed_candidate_requests_publication_before_later_failures() {
    let fixture = fixture().await;
    let broken = "zzz-lut";
    let base = native("1.0.0", 0.005);
    publish(
        &fixture.worker,
        LUT,
        1,
        &oracle_family_lut(
            LUT,
            &base,
            family(RENDERING, None),
            fixture.worker.config().seed,
        ),
    )
    .await;
    publish(&fixture.worker, broken, 1, &broken_lut_document(broken)).await;
    publish(&fixture.worker, BASE, 1, &base).await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;

    assert!(fixture.worker.run_until_idle().await.is_err());
    assert!(published(&fixture.worker, LUT, 2).await.is_matchable());
    let queued: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM jobs WHERE kind = 'publish_site' AND status = 'queued'",
    )
    .fetch_one(fixture.worker.pool())
    .await
    .expect("publish job");
    assert!(queued > 0);
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_base_published_before_selection_is_the_fitted_target() {
    let fixture = established().await;
    publish(
        &fixture.worker,
        BASE,
        2,
        &native_at("2.0.0", 0.005, "Fujifilm", RENDERING, Some(40.0)),
    )
    .await;
    publish(&fixture.worker, BASE, 3, &native("3.0.0", 0.005)).await;

    drain(&fixture.worker).await;

    assert!(
        !looklab_worker::db::gate_failure_pending(fixture.worker.pool(), LUT, 3)
            .await
            .expect("gate failure")
    );
    let document = published(&fixture.worker, LUT, 3).await;
    assert_eq!(
        document.base(),
        Some(&BaseReference::Look {
            slug: model_slug(BASE),
            version: LookVersion::new("3.0.0").expect("version"),
        })
    );
    assert!(document.is_matchable());
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_root_revoked_during_commit_does_not_publish() {
    let fixture = established().await;
    publish(&fixture.worker, BASE, 2, &native("2.0.0", 0.005)).await;
    let root = insert_root(&fixture.worker, BASE, 2).await;
    let trigger = format!(
        "CREATE TRIGGER revoke_during_candidate AFTER INSERT ON version_manifests
         WHEN (SELECT look FROM look_versions WHERE id = NEW.look_version_id) = '{LUT}'
         BEGIN UPDATE root_admissions SET revoked_at = 1 WHERE id = '{}'; END",
        root.as_str()
    );
    sqlx::query(trigger.as_str())
        .execute(fixture.worker.pool())
        .await
        .expect("trigger");

    drain(&fixture.worker).await;

    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 3)
            .await
            .expect("manifest")
            .is_none()
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 2)
    );
    assert!(
        admin::invalidated_versions(fixture.worker.pool())
            .await
            .expect("invalid")
            .is_empty()
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn an_obsolete_gate_failure_keeps_the_newer_work() {
    let fixture = established().await;
    publish(
        &fixture.worker,
        BASE,
        2,
        &native_at("2.0.0", 0.005, "Fujifilm", RENDERING, Some(40.0)),
    )
    .await;
    let newer = native("3.0.0", 0.005);
    let newer_bytes = newer.encode().expect("encode");
    let newer_sha = hex(&Sha256::digest(&newer_bytes));
    std::fs::write(fixture.worker.config().blobs.join(&newer_sha), &newer_bytes).expect("blob");
    let artifacts = format!(r#"[{{"name":"look.looklab","sha256":"{newer_sha}"}}]"#);
    let trigger = format!(
        "CREATE TRIGGER newer_base_on_hold AFTER INSERT ON gate_failures
         WHEN NEW.look = '{LUT}'
         BEGIN
           INSERT INTO look_versions (id, look, version, status, created_at)
           VALUES ('race-base-v3', '{BASE}', 3, 'published', 0);
           INSERT INTO version_manifests (look_version_id, artifacts, created_at)
           VALUES ('race-base-v3', '{artifacts}', 0);
         END"
    );
    sqlx::query(trigger.as_str())
        .execute(fixture.worker.pool())
        .await
        .expect("trigger");

    drain(&fixture.worker).await;

    assert!(
        !looklab_worker::db::gate_failure_pending(fixture.worker.pool(), LUT, 3)
            .await
            .expect("gate failure")
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 2)
    );

    sqlx::query("DROP TRIGGER newer_base_on_hold")
        .execute(fixture.worker.pool())
        .await
        .expect("drop trigger");
    publish(&fixture.worker, BASE, 3, &newer).await;
    drain(&fixture.worker).await;

    assert!(published(&fixture.worker, LUT, 3).await.is_matchable());
    assert_eq!(
        published(&fixture.worker, LUT, 3).await.base(),
        Some(&BaseReference::Look {
            slug: model_slug(BASE),
            version: LookVersion::new("3.0.0").expect("version"),
        })
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn losing_the_lease_during_fit_does_not_hold_work() {
    let fixture = established().await;
    publish(
        &fixture.worker,
        BASE,
        2,
        &native_at("2.0.0", 0.005, "Fujifilm", RENDERING, Some(40.0)),
    )
    .await;
    let claimed = looklab_worker::jobs::claim(
        fixture.worker.pool(),
        3_600,
        looklab_worker::clock::now_unix(),
    )
    .await
    .expect("claim")
    .expect("job");
    let ownership = claimed.ownership();
    let trigger = format!(
        "CREATE TRIGGER lease_loss_on_hold AFTER INSERT ON gate_failures
         WHEN NEW.look = '{LUT}'
         BEGIN UPDATE jobs SET attempts = attempts + 100 WHERE id = '{}'; END",
        ownership.id.as_str()
    );
    sqlx::query(trigger.as_str())
        .execute(fixture.worker.pool())
        .await
        .expect("trigger");

    assert!(
        looklab_worker::lut_rebase::run(fixture.worker.pool(), fixture.worker.config(), &ownership)
            .await
            .is_err()
    );
    assert!(
        !looklab_worker::db::gate_failure_pending(fixture.worker.pool(), LUT, 3)
            .await
            .expect("gate failure")
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == LUT && work.version == 2)
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_matching_family_look_publishing_during_fit_keeps_work() {
    let fixture = fixture().await;
    let source = "family-lut";
    let other = "other-native";
    let measured = native("1.0.0", 0.005);
    publish(
        &fixture.worker,
        source,
        1,
        &oracle_family_lut(
            source,
            &measured,
            family(RENDERING, None),
            fixture.worker.config().seed,
        ),
    )
    .await;
    admin::record_native_rendering(
        fixture.worker.pool(),
        &look_slug(other),
        1,
        &CameraRendering {
            maker: "Fujifilm".to_owned(),
            name: RENDERING.to_owned(),
        },
    )
    .await
    .expect("rendering");
    publish(
        &fixture.worker,
        BASE,
        1,
        &native_at("1.0.0", 0.005, "Fujifilm", RENDERING, Some(40.0)),
    )
    .await;

    let candidate = native_document(other, "1.0.0", 0.005, "Fujifilm", RENDERING, None);
    let candidate_bytes = candidate.encode().expect("encode");
    let candidate_sha = hex(&Sha256::digest(&candidate_bytes));
    std::fs::write(
        fixture.worker.config().blobs.join(&candidate_sha),
        &candidate_bytes,
    )
    .expect("blob");
    let artifacts = format!(r#"[{{"name":"look.looklab","sha256":"{candidate_sha}"}}]"#);
    let trigger = format!(
        "CREATE TRIGGER family_candidate_on_hold AFTER INSERT ON gate_failures
         WHEN NEW.look = '{source}'
         BEGIN
           INSERT INTO look_versions (id, look, version, status, created_at)
           VALUES ('race-other-v1', '{other}', 1, 'published', 0);
           INSERT INTO version_manifests (look_version_id, artifacts, created_at)
           VALUES ('race-other-v1', '{artifacts}', 0);
         END"
    );
    sqlx::query(trigger.as_str())
        .execute(fixture.worker.pool())
        .await
        .expect("trigger");

    drain(&fixture.worker).await;

    assert!(
        !looklab_worker::db::gate_failure_pending(fixture.worker.pool(), source, 2)
            .await
            .expect("gate failure")
    );
    assert!(
        admin::pending_lut_rebases(fixture.worker.pool())
            .await
            .expect("pending")
            .iter()
            .any(|work| work.look.as_str() == source && work.version == 1)
    );

    sqlx::query("DROP TRIGGER family_candidate_on_hold")
        .execute(fixture.worker.pool())
        .await
        .expect("drop trigger");
    publish(
        &fixture.worker,
        other,
        2,
        &native_document(other, "2.0.0", 0.005, "Fujifilm", RENDERING, None),
    )
    .await;
    drain(&fixture.worker).await;

    let document = published(&fixture.worker, source, 2).await;
    assert_eq!(
        document.base(),
        Some(&BaseReference::Look {
            slug: model_slug(other),
            version: LookVersion::new("2.0.0").expect("version"),
        })
    );
}

#[tokio::test]
#[cfg(feature = "integration")]
async fn a_failed_rebase_gate_keeps_the_old_version() {
    let fixture = established().await;
    let old = artifact_bytes(&fixture.worker, LUT, 2).await;

    publish(
        &fixture.worker,
        BASE,
        2,
        &native_at("2.0.0", 0.005, "Fujifilm", RENDERING, Some(40.0)),
    )
    .await;
    drain(&fixture.worker).await;

    assert!(
        looklab_worker::db::gate_failure_pending(fixture.worker.pool(), LUT, 3)
            .await
            .expect("gate failure")
    );
    assert!(
        looklab_worker::db::manifest_artifacts(fixture.worker.pool(), LUT, 3)
            .await
            .expect("manifest")
            .is_none()
    );
    assert_eq!(artifact_bytes(&fixture.worker, LUT, 2).await, old);
    assert!(published(&fixture.worker, LUT, 2).await.is_matchable());
}
