// SPDX-License-Identifier: AGPL-3.0-or-later

//! Approved interface definitions publish with their bound reference report.

use std::path::{Path, PathBuf};

use looklab_publish::{
    ArtifactHash, CctSummary, InterfaceProfileManifest, InterfaceProvenance, RoundTripReport,
    SiteIndex,
};
use looklab_schema::{
    ProposalKind, ProposalStatus, SNAPSHOT_MANIFEST, SnapshotManifest, content_hash,
    interface_export_path, validate_interface_profile,
};
use looklab_worker::WorkerError;
use looklab_worker::publish::{PublishConfig, publish_with_pool};
use tempfile::TempDir;

const DEFINITION: &str = include_str!("../../../data/interfaces/example-app-lut3d-v1.yaml");

#[derive(Clone, Copy)]
enum Publication {
    Pending,
    Published,
}

impl Publication {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Published => "published",
        }
    }
}

struct Fixture {
    _dir: TempDir,
    data: PathBuf,
    site: PathBuf,
    blobs: PathBuf,
    transforms: PathBuf,
    db: PathBuf,
    snapshot_dir: PathBuf,
    snapshot_hash: String,
}

impl Fixture {
    fn new() -> Self {
        let profile = validate_interface_profile(DEFINITION).expect("definition");
        Self::new_with_files(vec![(
            interface_export_path(&profile.id),
            DEFINITION.as_bytes().to_vec(),
        )])
    }

    fn new_with_body() -> Self {
        let body_text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../data/bodies/fujifilm-x-t4.body.yaml"),
        )
        .expect("body");
        let schema_text = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/bodies/fujifilm-x-t4.yaml"),
        )
        .expect("schema");
        let body = looklab_schema::validate_body(&body_text).expect("body");
        let schema = looklab_schema::validate_control_schema(&schema_text).expect("schema");
        Self::new_with_files(vec![
            (
                looklab_schema::body_export_path(&body.id),
                body_text.into_bytes(),
            ),
            (
                looklab_schema::schema_export_path(&schema.body),
                schema_text.into_bytes(),
            ),
        ])
    }

    fn new_with_files(files: Vec<(String, Vec<u8>)>) -> Self {
        let dir = TempDir::new().expect("temp dir");
        let data = dir.path().join("data");
        std::fs::create_dir_all(data.join("bodies")).expect("bodies");
        std::fs::create_dir_all(data.join("card/conformance-v1")).expect("conformance");
        let blobs = dir.path().join("blobs");
        std::fs::create_dir_all(&blobs).expect("blobs");
        let transforms = dir.path().join("transforms");
        std::fs::create_dir_all(&transforms).expect("transforms");
        let site = dir.path().join("site");
        let db = dir.path().join("looklab.sqlite3");
        let snapshot_dir = dir.path().join("community");
        let snapshot_hash = write_snapshot(&snapshot_dir, 1, &files);
        Self {
            _dir: dir,
            data,
            site,
            blobs,
            transforms,
            db,
            snapshot_dir,
            snapshot_hash,
        }
    }

    fn write_snapshot(&self, version: u64, files: &[(String, Vec<u8>)]) -> String {
        write_snapshot(&self.snapshot_dir, version, files)
    }

    async fn publish(&self) -> Result<(), WorkerError> {
        self.publish_to(&self.site).await
    }

    async fn publish_to(&self, out: &Path) -> Result<(), WorkerError> {
        let pool = looklab_server::open_database(&self.db)
            .await
            .expect("database");
        let config = PublishConfig {
            db: self.db.clone(),
            blobs: self.blobs.clone(),
            data: self.data.clone(),
            manuals: self.data.join("manuals/catalog.yaml"),
            snapshot_dir: self.snapshot_dir.clone(),
            transforms: self.transforms.clone(),
            out: out.to_owned(),
        };
        publish_with_pool(&pool, &config).await.map(|_| ())
    }

    async fn activate(&self) {
        self.activate_version(1, &self.snapshot_hash).await;
    }

    async fn activate_version(&self, version: u64, hash: &str) {
        let pool = looklab_server::open_database(&self.db)
            .await
            .expect("database");
        sqlx::query(
            "INSERT INTO community_snapshot_activation
             (id, version, content_hash, input_version, input_hash, activated_at)
             VALUES (1, ?, ?, NULL, NULL, 0)
             ON CONFLICT (id) DO UPDATE SET
               version = excluded.version,
               content_hash = excluded.content_hash,
               input_version = NULL,
               input_hash = NULL,
               activated_at = excluded.activated_at",
        )
        .bind(i64::try_from(version).expect("version"))
        .bind(hash)
        .execute(&pool)
        .await
        .expect("activation");
    }

    async fn proposal(
        &self,
        id: &str,
        status: ProposalStatus,
        publication: Option<Publication>,
        report: Option<&str>,
    ) {
        self.proposal_at(
            id,
            status,
            publication,
            report,
            DEFINITION,
            1,
            &self.snapshot_hash,
        )
        .await;
    }

    #[allow(clippy::too_many_arguments)]
    async fn proposal_at(
        &self,
        id: &str,
        status: ProposalStatus,
        publication: Option<Publication>,
        report: Option<&str>,
        definition: &str,
        version: u64,
        hash: &str,
    ) {
        let pool = looklab_server::open_database(&self.db)
            .await
            .expect("database");
        let published_version = publication.map(|_| i64::try_from(version).expect("version"));
        let published_hash = publication.map(|_| hash);
        sqlx::query(
            "INSERT INTO proposals
             (id, credential_id, origin, type, payload, human_summary, evidence, status,
              created_at, round_trip_report, publication, published_version, published_hash)
             VALUES (?, NULL, 'worker', ?, ?, 'summary', '[]', ?, 0, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(ProposalKind::InterfaceProfile.as_str())
        .bind(definition)
        .bind(status.as_str())
        .bind(report)
        .bind(publication.map(Publication::as_str))
        .bind(published_version)
        .bind(published_hash)
        .execute(&pool)
        .await
        .expect("proposal");
    }
}

fn write_snapshot(root: &Path, version: u64, files: &[(String, Vec<u8>)]) -> String {
    let hash = content_hash(files);
    let target = root.join(format!("v{version}"));
    std::fs::create_dir_all(target.join("bodies")).expect("snapshot bodies");
    for (path, bytes) in files {
        let file = target.join(path.as_str());
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).expect("snapshot parent");
        }
        std::fs::write(file, bytes.as_slice()).expect("snapshot file");
    }
    std::fs::write(
        target.join(SNAPSHOT_MANIFEST),
        serde_json::to_vec(&SnapshotManifest {
            version,
            content_hash: hash.clone(),
        })
        .expect("manifest"),
    )
    .expect("snapshot manifest");
    hash
}

fn report_json() -> String {
    serde_json::to_string(&RoundTripReport {
        probes: 1057,
        target_clipped: 2,
        ccts: vec![CctSummary {
            cct: 6500.0,
            count: 1057,
            median: 0.2,
            p95: 0.6,
            max: 1.1,
            unsupported: 0,
            clipped: 2,
        }],
        flagged: false,
    })
    .expect("report")
}

fn index(fixture: &Fixture) -> SiteIndex {
    index_at(&fixture.site)
}

fn index_at(out: &Path) -> SiteIndex {
    serde_json::from_slice(&std::fs::read(out.join("data/index.json")).expect("index bytes"))
        .expect("index")
}

fn manifest(path: &Path) -> InterfaceProfileManifest {
    serde_json::from_slice(&std::fs::read(path).expect("manifest bytes")).expect("manifest")
}

#[tokio::test]
async fn only_approved_published_definitions_reach_the_catalog() {
    let fixture = Fixture::new();
    fixture.activate().await;
    let report = report_json();
    fixture
        .proposal(
            "approved-proposal",
            ProposalStatus::Approved,
            Some(Publication::Published),
            Some(report.as_str()),
        )
        .await;
    fixture
        .proposal(
            "pending-proposal",
            ProposalStatus::Pending,
            None,
            Some(report.as_str()),
        )
        .await;
    fixture
        .proposal(
            "rejected-proposal",
            ProposalStatus::Rejected,
            None,
            Some(report.as_str()),
        )
        .await;
    fixture
        .proposal(
            "pending-publication",
            ProposalStatus::Approved,
            Some(Publication::Pending),
            Some(report.as_str()),
        )
        .await;
    fixture.publish().await.expect("publish");

    let index = index(&fixture);
    assert_eq!(index.interfaces.len(), 1);
    let entry = &index.interfaces[0];
    let manifest_path = fixture.site.join(entry.manifest.as_str());
    assert_eq!(
        entry.manifest_sha256,
        ArtifactHash::of(&std::fs::read(&manifest_path).expect("manifest bytes"))
    );
    let manifest = manifest(&manifest_path);
    assert_eq!(
        manifest.provenance,
        InterfaceProvenance::Approved {
            proposal: looklab_publish::ProposalRef::new("approved-proposal").expect("ref"),
            snapshot_version: 1,
            snapshot_sha256: ArtifactHash::parse(&fixture.snapshot_hash).expect("hash"),
        }
    );
    let dir = fixture
        .site
        .join("data/interfaces")
        .join(entry.manifest_sha256.as_str());
    let definition = std::fs::read(dir.join("definition.yaml")).expect("definition");
    assert_eq!(definition.as_slice(), DEFINITION.as_bytes());
    manifest
        .verify_definition(&definition)
        .expect("bound definition");
    let report = std::fs::read(dir.join("report.json")).expect("report");
    manifest.verify_report(&report).expect("bound report");
}

#[tokio::test]
async fn a_shipped_definition_publishes_as_bootstrap() {
    let fixture = Fixture::new();
    let profile = validate_interface_profile(DEFINITION).expect("definition");
    let path = interface_export_path(&profile.id);
    std::fs::create_dir_all(fixture.data.join("interfaces")).expect("shipped interfaces");
    std::fs::write(fixture.data.join(path.as_str()), DEFINITION).expect("shipped definition");
    fixture.activate().await;
    fixture.publish().await.expect("publish");

    let index = index(&fixture);
    assert_eq!(index.interfaces.len(), 1);
    let entry = &index.interfaces[0];
    let manifest_path = fixture.site.join(entry.manifest.as_str());
    let manifest = manifest(&manifest_path);
    assert_eq!(manifest.provenance, InterfaceProvenance::Bootstrap);
    let dir = fixture
        .site
        .join("data/interfaces")
        .join(entry.manifest_sha256.as_str());
    manifest
        .verify_report(&std::fs::read(dir.join("report.json")).expect("report"))
        .expect("reference report");
}

#[tokio::test]
async fn a_snapshot_only_pending_definition_does_not_become_public() {
    let fixture = Fixture::new();
    let profile = validate_interface_profile(DEFINITION).expect("definition");
    let path = interface_export_path(&profile.id);
    std::fs::create_dir_all(fixture.data.join("interfaces")).expect("shipped interfaces");
    std::fs::write(fixture.data.join(path.as_str()), DEFINITION).expect("shipped definition");
    let report = report_json();
    fixture
        .proposal(
            "approved-a",
            ProposalStatus::Approved,
            Some(Publication::Published),
            Some(report.as_str()),
        )
        .await;
    fixture.activate().await;
    let first_out = fixture._dir.path().join("out-first");
    fixture.publish_to(&first_out).await.expect("first publish");
    let first_index = index_at(&first_out);
    assert_eq!(first_index.interfaces.len(), 1);
    assert_eq!(
        first_index.interfaces[0].name,
        "Example editor 3D LUT module"
    );

    let pending = DEFINITION.replace(
        "Example editor 3D LUT module",
        "Example editor 3D LUT module v2",
    );
    let pending_profile = validate_interface_profile(&pending).expect("pending definition");
    let pending_path = interface_export_path(&pending_profile.id);
    let pending_hash = fixture.write_snapshot(2, &[(pending_path, pending.into_bytes())]);
    fixture.activate_version(2, &pending_hash).await;

    let second_out = fixture._dir.path().join("out-second");
    fixture
        .publish_to(&second_out)
        .await
        .expect("second publish");
    let second_index = index_at(&second_out);
    assert_eq!(second_index.interfaces.len(), 1);
    assert_eq!(
        second_index.interfaces[0].name,
        "Example editor 3D LUT module"
    );
    for entry in std::fs::read_dir(second_out.join("data/interfaces")).expect("interfaces") {
        let path = entry.expect("entry").path().join("manifest.json");
        let manifest = manifest(&path);
        assert_ne!(manifest.name, "Example editor 3D LUT module v2");
    }
}

#[tokio::test]
async fn a_replaced_bootstrap_definition_survives_a_fresh_output() {
    let fixture = Fixture::new();
    fixture.activate().await;
    let profile = validate_interface_profile(DEFINITION).expect("definition");
    let path = interface_export_path(&profile.id);
    std::fs::create_dir_all(fixture.data.join("interfaces")).expect("shipped interfaces");
    std::fs::write(fixture.data.join(path.as_str()), DEFINITION).expect("shipped definition");

    let first_out = fixture._dir.path().join("out-first");
    fixture.publish_to(&first_out).await.expect("first publish");
    let first_index = index_at(&first_out);
    assert_eq!(first_index.interfaces.len(), 1);
    let first_entry = first_index.interfaces[0].clone();
    let first_manifest =
        std::fs::read(first_out.join(first_entry.manifest.as_str())).expect("first manifest");
    let first_dir = first_out
        .join("data/interfaces")
        .join(first_entry.manifest_sha256.as_str());
    let first_definition = std::fs::read(first_dir.join("definition.yaml")).expect("definition");
    let first_report = std::fs::read(first_dir.join("report.json")).expect("report");

    let replacement = DEFINITION.replace(
        "Example editor 3D LUT module",
        "Example editor 3D LUT module v2",
    );
    let replacement_profile = validate_interface_profile(&replacement).expect("replacement");
    let replacement_path = interface_export_path(&replacement_profile.id);
    let replacement_hash =
        fixture.write_snapshot(2, &[(replacement_path, replacement.clone().into_bytes())]);
    fixture.activate_version(2, &replacement_hash).await;
    let report = report_json();
    fixture
        .proposal_at(
            "approved-replacement",
            ProposalStatus::Approved,
            Some(Publication::Published),
            Some(report.as_str()),
            &replacement,
            2,
            &replacement_hash,
        )
        .await;

    let second_out = fixture._dir.path().join("out-second");
    fixture
        .publish_to(&second_out)
        .await
        .expect("second publish");
    let second_index = index_at(&second_out);
    assert_eq!(second_index.interfaces.len(), 1);
    assert_eq!(
        second_index.interfaces[0].name,
        "Example editor 3D LUT module v2"
    );
    assert_eq!(
        std::fs::read(second_out.join(first_entry.manifest.as_str())).expect("old manifest"),
        first_manifest
    );
    let restored_dir = second_out
        .join("data/interfaces")
        .join(first_entry.manifest_sha256.as_str());
    assert_eq!(
        std::fs::read(restored_dir.join("definition.yaml")).expect("old definition"),
        first_definition
    );
    assert_eq!(
        std::fs::read(restored_dir.join("report.json")).expect("old report"),
        first_report
    );
}

#[tokio::test]
async fn a_replaced_input_transform_survives_a_fresh_output() {
    let fixture = Fixture::new_with_body();
    fixture.activate().await;
    let first = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/transforms/fujifilm-x-t4.yaml"),
    )
    .expect("transform");
    let second = first.replace("version: 1", "version: 2");
    let transform_path = fixture.transforms.join("fujifilm-x-t4.yaml");
    std::fs::write(&transform_path, first.as_bytes()).expect("first transform");

    let first_out = fixture._dir.path().join("out-first");
    fixture.publish_to(&first_out).await.expect("first publish");
    let first_index = index_at(&first_out);
    assert_eq!(first_index.input_transforms.len(), 1);
    let first_entry = first_index.input_transforms[0].clone();
    let first_bytes =
        std::fs::read(first_out.join(first_entry.path.as_str())).expect("first prior");
    assert_eq!(first_entry.sha256, ArtifactHash::of(&first_bytes));

    std::fs::write(&transform_path, second.as_bytes()).expect("second transform");
    let second_out = fixture._dir.path().join("out-second");
    fixture
        .publish_to(&second_out)
        .await
        .expect("second publish");
    let second_index = index_at(&second_out);
    assert_eq!(second_index.input_transforms.len(), 1);
    assert_ne!(second_index.input_transforms[0].sha256, first_entry.sha256);
    assert_eq!(
        std::fs::read(second_out.join(first_entry.path.as_str())).expect("old prior"),
        first_bytes
    );
    assert_eq!(
        std::fs::read(second_out.join(second_index.input_transforms[0].path.as_str()))
            .expect("current prior")
            .as_slice(),
        second.as_bytes()
    );
}

#[tokio::test]
async fn a_missing_or_corrupt_approved_report_fails_publication() {
    let fixture = Fixture::new();
    fixture.activate().await;
    fixture
        .proposal(
            "approved-proposal",
            ProposalStatus::Approved,
            Some(Publication::Published),
            None,
        )
        .await;
    let error = fixture.publish().await.expect_err("missing report");
    assert!(
        matches!(&error, WorkerError::Data(message) if message.contains("no reference round-trip report")),
        "{error}"
    );

    let pool = looklab_server::open_database(&fixture.db)
        .await
        .expect("database");
    sqlx::query("UPDATE proposals SET round_trip_report = ? WHERE id = 'approved-proposal'")
        .bind("{\"probes\":")
        .execute(&pool)
        .await
        .expect("update");
    let error = fixture.publish().await.expect_err("corrupt report");
    assert!(
        matches!(&error, WorkerError::Data(message) if message.contains("unreadable reference round-trip report")),
        "{error}"
    );
}
