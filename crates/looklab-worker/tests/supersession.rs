// SPDX-License-Identifier: AGPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;

use looklab_model::file::{
    BaseField, BaseReference, BaseStatus, CameraRendering, DeclaredEncoding, Gamut, LicenseRecord,
    LogTransfer, LutData, LutDefinition, LutEmulation, LutRecord, OutputEncoding,
};
use looklab_model::{Look, LookDocument, LookIdentity, LookKind, LookPin, WbPolicy};
use looklab_publish::{LookListing, LookPage, SiteIndex};
use looklab_server::admin::{self, ManifestArtifact};
use looklab_trust::VersionId;
use looklab_upload::LookSlug;
use looklab_worker::clock::now_unix;
use looklab_worker::jobs::{self, JobKind, PublicationGeneration};
use looklab_worker::{JobPayload, Worker, WorkerConfig, WorkerError};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use tempfile::TempDir;

const NATIVE: &str = "classic-chrome";
const MATCHING: &str = "classic-chrome-lut";
const OTHER: &str = "leica-lut";
const PREVIEW: &str = "preview-only";

/// Jobs one fixture queue may run before it must settle.
const DRAIN_JOBS: usize = 64;
const DRAIN_DEADLINE: Duration = Duration::from_secs(900);

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

    async fn worker(&self, site: &Path) -> Worker {
        let mut config = WorkerConfig::new(self.root.join("looklab.sqlite3"), self.blobs.clone());
        config.data = self.data.clone();
        config.transforms = self.root.join("transforms");
        config.site = site.to_owned();
        Worker::open(config).await.expect("worker")
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
        transforms: fixture.root.join("transforms"),
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

fn read_index(out: &Path) -> SiteIndex {
    serde_json::from_slice(&std::fs::read(out.join("data/index.json")).expect("index"))
        .expect("index json")
}

/// A manifest write queues a LUT rebase job; publication tests settle it first.
async fn drain_manifest_jobs(worker: &Worker) {
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("manifest jobs");
}

/// Claims the queued publisher before it takes its snapshot and writes.
async fn pause_publisher(worker: &Worker) -> (jobs::JobId, i64, PublicationGeneration) {
    jobs::request_publish(worker.pool(), 0)
        .await
        .expect("publication request");
    let observed = jobs::publication_generation(worker.pool())
        .await
        .expect("generation");
    let job = jobs::claim(worker.pool(), 3_600, now_unix())
        .await
        .expect("claim")
        .expect("queued publisher");
    assert_eq!(job.kind, JobKind::PublishSite);
    (job.id, job.attempts, observed)
}

#[tokio::test]
async fn a_running_publisher_blocks_another_publish_claim() {
    let fixture = fixture().await;
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
    let site = fixture.publish_root("drain");
    std::fs::create_dir_all(&site).expect("site root");
    let worker = fixture.worker(&site.join("data")).await;
    drain_manifest_jobs(&worker).await;

    let (job, attempts, observed) = pause_publisher(&worker).await;
    worker
        .enqueue(&JobPayload::PublishSite, 0)
        .await
        .expect("second publisher");
    assert!(
        jobs::claim(worker.pool(), 3_600, now_unix())
            .await
            .expect("claim")
            .is_none(),
        "the running publisher holds the output"
    );

    assert!(
        jobs::complete_publish(worker.pool(), &job, attempts, observed, now_unix())
            .await
            .expect("complete")
    );
    let next = jobs::claim(worker.pool(), 3_600, now_unix())
        .await
        .expect("claim")
        .expect("publish job");
    assert_eq!(next.kind, JobKind::PublishSite);
}

#[tokio::test]
async fn a_superseded_claim_cannot_overwrite_a_newer_publisher() {
    let fixture = fixture().await;
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
    let site = fixture.publish_root("drain");
    std::fs::create_dir_all(&site).expect("site root");
    let out = site.join("data");
    let worker = fixture.worker(&out).await;
    drain_manifest_jobs(&worker).await;
    let moderator = credential(worker.pool()).await;
    let slug = LookSlug::try_from(OTHER.to_owned()).expect("slug");

    jobs::request_publish(worker.pool(), 0)
        .await
        .expect("publication request");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("initial publication");
    assert!(page(&out, OTHER).is_some());

    let (stale_job, stale_attempts, _observed) = pause_publisher(&worker).await;
    let stale = jobs::JobOwnership {
        id: stale_job.clone(),
        attempts: stale_attempts,
    };

    sqlx::query("UPDATE jobs SET locked_until = 0 WHERE id = ?")
        .bind(stale_job.as_str())
        .execute(worker.pool())
        .await
        .expect("expire lease");
    admin::hide_look(
        worker.pool(),
        &moderator,
        &slug,
        "the donor asked for removal",
    )
    .await
    .expect("hide");
    assert_eq!(
        worker
            .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
            .await
            .expect("second publisher"),
        1
    );
    assert!(
        !read_index(&out)
            .looks
            .iter()
            .any(|look| look.slug.as_str() == OTHER)
    );
    assert!(page(&out, OTHER).is_none());

    admin::relist_look(
        worker.pool(),
        &moderator,
        &slug,
        "the donor changed their mind",
    )
    .await
    .expect("relist");
    let error = worker
        .publish_owned(&stale)
        .await
        .expect_err("superseded claim");
    assert!(matches!(error, WorkerError::Job(_)), "{error}");
    assert!(
        !read_index(&out)
            .looks
            .iter()
            .any(|look| look.slug.as_str() == OTHER)
    );
    assert!(page(&out, OTHER).is_none());
}

#[tokio::test]
async fn a_second_publisher_waits_for_the_output_lock() {
    let fixture = fixture().await;
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
    let (index, out) = publish(&fixture, "site").await;
    assert!(index.looks.iter().any(|look| look.slug.as_str() == OTHER));
    let moderator = credential(&fixture.pool).await;
    admin::hide_look(
        &fixture.pool,
        &moderator,
        &LookSlug::try_from(OTHER.to_owned()).expect("slug"),
        "a rights claim",
    )
    .await
    .expect("hide");

    let config = looklab_worker::publish::PublishConfig {
        db: fixture.root.join("looklab.sqlite3"),
        blobs: fixture.blobs.clone(),
        data: fixture.data.clone(),
        transforms: fixture.root.join("transforms"),
        out: out.clone(),
    };
    let held = config.lock_output().expect("output lock");
    let publisher_config = config.clone();
    let (started_tx, started_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let publisher = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        started_tx.send(()).expect("started");
        let published = runtime.block_on(looklab_worker::publish_site(&publisher_config));
        done_tx.send(published.is_ok()).expect("done");
    });

    started_rx.recv().expect("started");
    assert!(
        matches!(done_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "published while another publisher held the output lock"
    );
    drop(held);
    assert!(
        done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("second publisher")
    );
    publisher.join().expect("publisher");
    assert!(
        !read_index(&out)
            .looks
            .iter()
            .any(|look| look.slug.as_str() == OTHER)
    );
    assert!(page(&out, OTHER).is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn a_waiting_publisher_leaves_the_current_thread_runtime_free() {
    let fixture = fixture().await;
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
    let (index, out) = publish(&fixture, "site").await;
    assert!(index.looks.iter().any(|look| look.slug.as_str() == OTHER));
    let moderator = credential(&fixture.pool).await;
    admin::hide_look(
        &fixture.pool,
        &moderator,
        &LookSlug::try_from(OTHER.to_owned()).expect("slug"),
        "a rights claim",
    )
    .await
    .expect("hide");

    let config = looklab_worker::publish::PublishConfig {
        db: fixture.root.join("unused.sqlite3"),
        blobs: fixture.blobs.clone(),
        data: fixture.data.clone(),
        transforms: fixture.root.join("transforms"),
        out: out.clone(),
    };
    let held = config.lock_output().expect("output lock");
    let (progress_tx, progress_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let publish_config = config.clone();
    let pool = fixture.pool.clone();
    let runtime_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let outcome = runtime.block_on(async move {
            let mut publishing = Box::pin(looklab_worker::publish::publish_with_pool(
                &pool,
                &publish_config,
            ));
            let (signal_tx, signal_rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = signal_tx.send(());
            });
            tokio::select! {
                biased;
                result = &mut publishing => {
                    return Err(format!("published while the output lock was held: {result:?}"));
                }
                _ = signal_rx => {}
            }
            let _ = progress_tx.send(());
            publishing
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        });
        done_tx.send(outcome).expect("done");
    });

    let progressed = progress_rx.recv_timeout(Duration::from_secs(10)).is_ok();
    drop(held);
    let outcome = done_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("publisher finished");
    runtime_thread.join().expect("runtime thread");
    assert!(
        progressed,
        "the current-thread runtime made no progress while the publisher waited"
    );
    outcome.expect("publish after the lock");
    assert!(
        !read_index(&out)
            .looks
            .iter()
            .any(|look| look.slug.as_str() == OTHER)
    );
    assert!(page(&out, OTHER).is_none());
}

#[tokio::test]
async fn a_permanent_publication_failure_waits_for_a_new_request() {
    let fixture = fixture().await;
    let document = lut_document(
        OTHER,
        LutEmulation::CameraRendering {
            maker: "leica".to_owned(),
            rendering: "classic".to_owned(),
        },
    );
    add_look(&fixture, OTHER, &document).await;
    let site = fixture.publish_root("drain");
    std::fs::create_dir_all(&site).expect("site root");
    let out = site.join("data");
    let worker = fixture.worker(&out).await;
    drain_manifest_jobs(&worker).await;

    let blob = fixture.blobs.join(hex(&Sha256::digest(&document)));
    std::fs::write(&blob, b"corrupt").expect("corrupt blob");

    jobs::request_publish(worker.pool(), 0)
        .await
        .expect("publication request");
    let error = worker.run_once().await.expect_err("hash mismatch");
    assert!(matches!(error, WorkerError::Data(_)), "{error}");

    for _ in 0..5 {
        assert!(worker.run_once().await.expect("idle").is_none());
    }
    let jobs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE kind = 'publish_site'")
        .fetch_one(worker.pool())
        .await
        .expect("jobs");
    assert_eq!(
        jobs, 1,
        "a terminal failure must not spawn replacement publishers"
    );
    let (status, attempts): (String, i64) =
        sqlx::query_as("SELECT status, attempts FROM jobs WHERE kind = 'publish_site'")
            .fetch_one(worker.pool())
            .await
            .expect("job row");
    assert_eq!((status.as_str(), attempts), ("failed", 1));

    std::fs::write(&blob, &document).expect("repair blob");
    jobs::request_publish(worker.pool(), 0)
        .await
        .expect("repair request");
    assert_eq!(
        worker
            .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
            .await
            .expect("repaired publication"),
        1
    );
    assert!(page(&out, OTHER).is_some());
    assert!(
        read_index(&out)
            .looks
            .iter()
            .any(|look| look.slug.as_str() == OTHER)
    );
}

#[tokio::test]
async fn a_hide_during_publication_reaches_the_site_without_another_request() {
    let fixture = fixture().await;
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
    let site = fixture.publish_root("drain");
    std::fs::create_dir_all(&site).expect("site root");
    let out = site.join("data");
    let worker = fixture.worker(&out).await;
    drain_manifest_jobs(&worker).await;
    let moderator = credential(worker.pool()).await;
    let slug = LookSlug::try_from(OTHER.to_owned()).expect("slug");

    jobs::request_publish(worker.pool(), 0)
        .await
        .expect("publication request");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("initial publication");
    assert!(page(&out, OTHER).is_some());

    let (job, attempts, observed) = pause_publisher(&worker).await;

    admin::hide_look(
        worker.pool(),
        &moderator,
        &slug,
        "the donor asked for removal",
    )
    .await
    .expect("hide");
    assert!(
        worker.run_once().await.expect("claim").is_none(),
        "a publisher is already running"
    );
    assert!(page(&out, OTHER).is_some());

    assert!(
        jobs::complete_publish(worker.pool(), &job, attempts, observed, now_unix())
            .await
            .expect("complete")
    );
    assert_eq!(
        worker
            .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
            .await
            .expect("successor"),
        1
    );

    let index = read_index(&out);
    assert!(!index.looks.iter().any(|look| look.slug.as_str() == OTHER));
    assert!(page(&out, OTHER).is_none());
    assert!(!out.join(format!("data/looks/{OTHER}")).exists());
}

#[tokio::test]
async fn a_relist_during_publication_restores_the_site_without_another_request() {
    let fixture = fixture().await;
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
    let site = fixture.publish_root("drain");
    std::fs::create_dir_all(&site).expect("site root");
    let out = site.join("data");
    let worker = fixture.worker(&out).await;
    drain_manifest_jobs(&worker).await;
    let moderator = credential(worker.pool()).await;
    let slug = LookSlug::try_from(OTHER.to_owned()).expect("slug");

    admin::hide_look(worker.pool(), &moderator, &slug, "a rights claim")
        .await
        .expect("hide");
    worker
        .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
        .await
        .expect("initial publication");
    assert!(page(&out, OTHER).is_none());

    let (job, attempts, observed) = pause_publisher(&worker).await;

    admin::relist_look(worker.pool(), &moderator, &slug, "the claim was withdrawn")
        .await
        .expect("relist");
    assert!(
        worker.run_once().await.expect("claim").is_none(),
        "a publisher is already running"
    );
    assert!(page(&out, OTHER).is_none());

    assert!(
        jobs::complete_publish(worker.pool(), &job, attempts, observed, now_unix())
            .await
            .expect("complete")
    );
    assert_eq!(
        worker
            .run_until_idle_bounded(DRAIN_JOBS, DRAIN_DEADLINE)
            .await
            .expect("successor"),
        1
    );

    let index = read_index(&out);
    assert!(index.looks.iter().any(|look| look.slug.as_str() == OTHER));
    assert_eq!(
        page(&out, OTHER).expect("restored page").listing,
        LookListing::Listed
    );
    assert!(
        out.join(format!("data/looks/{OTHER}/1.0.0/look.looklab"))
            .exists()
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
