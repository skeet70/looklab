// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};

use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, CameraRendering, DeclaredEncoding, Gamut, LicenseRecord,
    LogTransfer, LutData, LutDefinition, LutEmulation, LutRecord, OutputEncoding,
};
use looklab_model::{Look, LookDocument, LookIdentity, LookKind, LookPin, WbPolicy};
use looklab_publish::{LookListing, LookPage, SiteIndex};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_trust::VersionId;
use looklab_upload::LookSlug;
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;

const NATIVE: &str = "classic-chrome";
const MATCHING: &str = "classic-chrome-lut";
const OTHER: &str = "leica-lut";
const PREVIEW: &str = "preview-only";

struct Fixture {
    _dir: TempDir,
    pool: SqlitePool,
    blobs: PathBuf,
    data: PathBuf,
    root: PathBuf,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let pool = looklab_server::open_database(&dir.path().join("looklab.sqlite3"))
        .await
        .expect("database");
    let blobs = dir.path().join("blobs");
    std::fs::create_dir_all(&blobs).expect("blob dir");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data");
    let root = dir.path().to_owned();
    Fixture {
        _dir: dir,
        pool,
        blobs,
        data,
        root,
    }
}

impl Fixture {
    fn publish_root(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn lut_data() -> LutData {
    LutData::new(
        2,
        [0.0; 3],
        [1.0; 3],
        (0..24).map(|value| value as f64 / 23.0).collect(),
    )
    .expect("lut")
}

fn license() -> LicenseRecord {
    LicenseRecord {
        own_work: true,
        source_license: None,
        source_url: None,
        attribution: "a donor".to_owned(),
    }
}

fn lut_definition(name: &str, emulation: LutEmulation) -> LutDefinition {
    LutDefinition {
        name: name.to_owned(),
        license: license(),
        encoding: DeclaredEncoding::SceneReferred {
            transfer: LogTransfer::Flog2,
            gamut: Gamut::Fgamut,
        },
        output: OutputEncoding::Srgb,
        lut: LutRecord {
            size: 2,
            domain_min: [0.0; 3],
            domain_max: [1.0; 3],
            values_ref: looklab_model::BinRef::new("bin:lut").expect("ref"),
        },
        emulates: Some(emulation),
        superseded_by: None,
        notes: Vec::new(),
    }
}

fn lut_document(slug: &str, emulation: LutEmulation) -> Vec<u8> {
    let definition = lut_definition(slug, emulation);
    let identity =
        LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive).expect("id");
    LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut_data())
        .expect("document")
        .encode()
        .expect("encode")
}

fn preview_document(slug: &str) -> Vec<u8> {
    let mut definition = lut_definition(
        slug,
        LutEmulation::FilmStock {
            name: "Preview".to_owned(),
        },
    );
    definition.encoding = DeclaredEncoding::DisplayReferred {
        base: BaseReference::neutral_baseline(),
        status: BaseStatus::BaseIncomplete {
            missing: BaseField::FilmSimulation,
        },
    };
    let identity =
        LookIdentity::new(slug, "1.0.0", LookKind::LutDefined, WbPolicy::Adaptive).expect("id");
    LookDocument::from_lut_definition(identity, Look::colorimetric(), definition, lut_data())
        .expect("document")
        .encode()
        .expect("encode")
}

fn native_document(maker: &str, name: &str) -> Vec<u8> {
    let identity =
        LookIdentity::new(NATIVE, "1.0.0", LookKind::NativeFamily, WbPolicy::Adaptive).expect("id");
    let mut document = LookDocument::from_look(identity, Look::colorimetric()).expect("document");
    document.file.rendering = Some(CameraRendering {
        maker: maker.to_owned(),
        name: name.to_owned(),
    });
    document.encode().expect("encode")
}

async fn add_look(fixture: &Fixture, slug: &str, bytes: &[u8]) {
    let sha256 = hex(&Sha256::digest(bytes));
    std::fs::write(fixture.blobs.join(&sha256), bytes).expect("blob");
    admin::write_version_manifest(
        &fixture.pool,
        &LookSlug::try_from(slug.to_owned()).expect("slug"),
        VersionId::new(1),
        &[ManifestArtifact {
            name: "look.looklab".to_owned(),
            sha256,
        }],
        &[],
    )
    .await
    .expect("manifest");
}

async fn publish(fixture: &Fixture, name: &str) -> (SiteIndex, PathBuf) {
    let out = fixture.publish_root(name).join("data");
    std::fs::create_dir_all(out.parent().expect("root")).expect("site root");
    let config = looklab_worker::publish::PublishConfig {
        db: fixture.root.join("unused.sqlite3"),
        blobs: fixture.blobs.clone(),
        data: fixture.data.clone(),
        out: out.clone(),
    };
    looklab_worker::publish::publish_with_pool(&fixture.pool, &config)
        .await
        .expect("publish");
    let index: SiteIndex =
        serde_json::from_slice(&std::fs::read(out.join("data/index.json")).expect("index"))
            .expect("index json");
    (index, out)
}

fn page(out: &Path, slug: &str) -> Option<LookPage> {
    let path = out.join(format!("data/looks/{slug}.json"));
    std::fs::read(path)
        .ok()
        .map(|bytes| serde_json::from_slice(&bytes).expect("page json"))
}

#[tokio::test]
async fn publishing_a_matching_native_look_supersedes_the_lut_and_a_moderator_can_hide_and_relist()
{
    let fixture = fixture().await;
    add_look(
        &fixture,
        NATIVE,
        &native_document("Fujifilm", "Classic Chrome"),
    )
    .await;
    add_look(
        &fixture,
        MATCHING,
        &lut_document(
            MATCHING,
            LutEmulation::CameraRendering {
                maker: "fujifilm".to_owned(),
                rendering: "classic chrome".to_owned(),
            },
        ),
    )
    .await;
    add_look(
        &fixture,
        OTHER,
        &lut_document(
            OTHER,
            LutEmulation::CameraRendering {
                maker: "leica".to_owned(),
                rendering: "classic".to_owned(),
            },
        ),
    )
    .await;
    add_look(&fixture, PREVIEW, &preview_document(PREVIEW)).await;

    let (index, out) = publish(&fixture, "site").await;
    assert!(
        index
            .looks
            .iter()
            .any(|look| look.slug.as_str() == MATCHING)
    );
    let matching_summary = index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == MATCHING)
        .expect("matching summary");
    assert!(matching_summary.latest().expect("version").matchable);
    let preview_summary = index
        .looks
        .iter()
        .find(|look| look.slug.as_str() == PREVIEW)
        .expect("preview summary");
    assert!(!preview_summary.latest().expect("version").matchable);
    let superseded = page(&out, MATCHING).expect("lut page");
    assert_eq!(superseded.listing, LookListing::Listed);
    assert_eq!(
        superseded.superseded_by,
        Some(LookPin::new(NATIVE, "1.0.0").expect("pin"))
    );
    let preview = page(&out, PREVIEW).expect("preview page");
    assert_eq!(preview.listing, LookListing::Listed);
    assert!(!preview.versions[0].matchable);
    assert!(
        out.join(format!("data/looks/{MATCHING}/1.0.0/look.looklab"))
            .exists()
    );
    let other = page(&out, OTHER).expect("other page");
    assert_eq!(other.superseded_by, None);
    let written: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM look_listings WHERE look = ?")
        .bind(OTHER)
        .fetch_one(&fixture.pool)
        .await
        .expect("listing row");
    assert_eq!(written, 0);

    let moderator = credential(&fixture.pool).await;
    admin::hide_look(
        &fixture.pool,
        &moderator,
        &LookSlug::try_from(MATCHING).expect("slug"),
        "superseded by a native Look",
    )
    .await
    .expect("hide");
    let hidden: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM audit_log WHERE type = 'look_hidden'")
            .fetch_one(&fixture.pool)
            .await
            .expect("audit rows");
    assert_eq!(hidden, 1);

    let (index, out) = publish(&fixture, "site").await;
    assert!(
        !index
            .looks
            .iter()
            .any(|look| look.slug.as_str() == MATCHING)
    );
    assert!(page(&out, MATCHING).is_none());
    assert!(
        !out.join(format!("data/looks/{MATCHING}/1.0.0/look.looklab"))
            .exists()
    );
    let log: looklab_publish::AuditLog =
        serde_json::from_slice(&std::fs::read(out.join("data/audit-log.json")).expect("log"))
            .expect("log json");
    assert!(log.entries.iter().any(|entry| matches!(
        &entry.event,
        looklab_publish::AuditEvent::LookListing {
            state: LookListing::Hidden,
            ..
        }
    )));

    admin::relist_look(
        &fixture.pool,
        &moderator,
        &LookSlug::try_from(MATCHING).expect("slug"),
        "the native Look is withdrawn",
    )
    .await
    .expect("relist");
    let (index, out) = publish(&fixture, "site").await;
    assert!(
        index
            .looks
            .iter()
            .any(|look| look.slug.as_str() == MATCHING)
    );
    assert_eq!(
        page(&out, MATCHING).expect("lut page").superseded_by,
        Some(LookPin::new(NATIVE, "1.0.0").expect("pin"))
    );
}

async fn credential(db: &SqlitePool) -> looklab_server::CredentialId {
    let id = looklab_server::CredentialId::new();
    sqlx::query(
        "INSERT INTO credentials
         (id, credential_id, webauthn_credential, tier, roles, reputation, created_at, status)
         VALUES (?, ?, '{}', 'new', '[\"moderator\"]', 0.5, 0, 'active')",
    )
    .bind(id.as_str())
    .bind(id.as_str().as_bytes())
    .execute(db)
    .await
    .expect("credential");
    id
}
