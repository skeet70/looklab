// SPDX-License-Identifier: AGPL-3.0-or-later
//! Behavioral tests for diagnostic retention, retrieval and the operator CLI.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use looklab_server::client_errors::{self, BuildId, ClientErrorReport};
use looklab_server::open_database;
use serde_json::{Value, json};
use sqlx::SqlitePool;
use tempfile::TempDir;

const RELEASE_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const RELEASE_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        .try_into()
        .expect("seconds")
}

fn report(release: &str, page: &str) -> ClientErrorReport {
    let value = json!({
        "version": 1,
        "kind": "exception",
        "error_type": "RangeError",
        "page": page,
        "release": release,
        "frames": [{"asset": "pkg/looklab-site.js", "line": 123, "column": 45}],
    });
    ClientErrorReport::parse(&serde_json::to_vec(&value).expect("json")).expect("report")
}

async fn database() -> (TempDir, SqlitePool, PathBuf) {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join("looklab.sqlite3");
    let db = open_database(&path).await.expect("database");
    (dir, db, path)
}

async fn count(db: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM client_errors")
        .fetch_one(db)
        .await
        .expect("row count")
}

async fn bulk_insert(db: &SqlitePool, base: i64, rows: i64) {
    let mut tx = db.begin().await.expect("transaction");
    for index in 0..rows {
        sqlx::query(
            "INSERT INTO client_errors (received_at, release, kind, error_type, page, frames)
             VALUES (?, ?, 'console_error', 'Error', 'other', '[]')",
        )
        .bind(base + index)
        .bind(RELEASE_B)
        .execute(&mut *tx)
        .await
        .expect("bulk row");
    }
    tx.commit().await.expect("commit");
}

#[tokio::test]
async fn reads_exclude_expired_reports_without_deleting_them() {
    let (_dir, db, _) = database().await;
    let now = now();
    client_errors::insert(&db, &report(RELEASE_A, "home"), now)
        .await
        .expect("insert");
    sqlx::query(
        "INSERT INTO client_errors (received_at, release, kind, error_type, page, frames)
         VALUES (?, ?, 'console_error', 'Error', 'other', '[]')",
    )
    .bind(now - 15 * 86_400)
    .bind(RELEASE_B)
    .execute(&db)
    .await
    .expect("old row");
    let reports = client_errors::recent(&db, 100, None).await.expect("recent");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].release.as_str(), RELEASE_A);
    assert_eq!(count(&db).await, 2);
}

#[tokio::test]
async fn insert_prunes_expired_reports() {
    let (_dir, db, _) = database().await;
    let now = now();
    sqlx::query(
        "INSERT INTO client_errors (received_at, release, kind, error_type, page, frames)
         VALUES (?, ?, 'console_error', 'Error', 'other', '[]')",
    )
    .bind(now - 15 * 86_400)
    .bind(RELEASE_B)
    .execute(&db)
    .await
    .expect("old row");
    client_errors::insert(&db, &report(RELEASE_A, "home"), now)
        .await
        .expect("insert");
    assert_eq!(count(&db).await, 1);
    let reports = client_errors::recent(&db, 100, None).await.expect("recent");
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].release.as_str(), RELEASE_A);
}

#[tokio::test]
async fn insert_keeps_the_latest_ten_thousand_reports() {
    let (_dir, db, _) = database().await;
    let now = now();
    let base = now - 20_000;
    bulk_insert(&db, base, 10_001).await;
    client_errors::insert(&db, &report(RELEASE_A, "home"), now)
        .await
        .expect("insert");
    assert_eq!(count(&db).await, 10_000);
    let oldest: i64 = sqlx::query_scalar("SELECT MIN(received_at) FROM client_errors")
        .fetch_one(&db)
        .await
        .expect("oldest");
    assert_eq!(oldest, base + 2);
    let reports = client_errors::recent(&db, 1_000, None)
        .await
        .expect("recent");
    assert_eq!(reports.len(), 1_000);
    assert_eq!(reports[0].received_at, now);
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_inserts_keep_the_storage_bound() {
    let (_dir, db, _) = database().await;
    let now = now();
    bulk_insert(&db, now - 20_000, 9_999).await;
    let a = report(RELEASE_A, "home");
    let b = report(RELEASE_B, "looks");
    let (first, second) = tokio::join!(
        client_errors::insert(&db, &a, now),
        client_errors::insert(&db, &b, now),
    );
    first.expect("first insert");
    second.expect("second insert");
    assert_eq!(count(&db).await, 10_000);
}

#[tokio::test]
async fn reads_filter_by_release_and_bound_the_limit() {
    let (_dir, db, _) = database().await;
    let now = now();
    for (release, offset) in [(RELEASE_A, 30), (RELEASE_B, 20), (RELEASE_A, 10)] {
        client_errors::insert(&db, &report(release, "home"), now - offset)
            .await
            .expect("insert");
    }
    let all = client_errors::recent(&db, 100, None).await.expect("recent");
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].received_at, now - 10);
    assert_eq!(all[1].release.as_str(), RELEASE_B);
    assert_eq!(all[2].received_at, now - 30);
    let release_a = BuildId::parse(RELEASE_A).expect("build id");
    let a = client_errors::recent(&db, 100, Some(&release_a))
        .await
        .expect("recent");
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].received_at, now - 10);
    let release_b = BuildId::parse(RELEASE_B).expect("build id");
    let b = client_errors::recent(&db, 100, Some(&release_b))
        .await
        .expect("recent");
    assert_eq!(b.len(), 1);
    let limited = client_errors::recent(&db, 1, None).await.expect("recent");
    assert_eq!(limited.len(), 1);
    assert_eq!(limited[0].received_at, now - 10);
}

fn write_config(dir: &Path, db: &Path) -> PathBuf {
    let path = dir.join("looklab.toml");
    std::fs::write(&path, format!("db_path = \"{}\"\n", db.display())).expect("config");
    path
}

fn cli(config: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_looklab-server"));
    command
        .arg("client-errors")
        .arg("--config")
        .arg(config)
        .args(args);
    for name in [
        "LOOKLAB_BIND",
        "LOOKLAB_DB_PATH",
        "LOOKLAB_BLOB_DIR",
        "LOOKLAB_RP_ID",
        "LOOKLAB_ORIGIN",
        "LOOKLAB_SERVER_SECRET_PATH",
        "LOOKLAB_BODIES_DIR",
        "LOOKLAB_MANUALS_DIR",
        "LOOKLAB_SNAPSHOT_DIR",
        "LOOKLAB_NONCE_TTL_SECONDS",
        "LOOKLAB_REGISTRATION_TTL_SECONDS",
        "LOOKLAB_BODY_TIMEOUT_SECONDS",
        "LOOKLAB_BLOB_RETENTION_DAYS",
        "LOOKLAB_SITE_DIST",
        "LOOKLAB_SITE_DATA",
        "LOOKLAB_CORS_ORIGINS",
        "LOOKLAB_POW_MEMORY_KIB",
        "LOOKLAB_POW_ITERATIONS",
        "LOOKLAB_POW_DIFFICULTY_BITS",
    ] {
        command.env_remove(name);
    }
    command.output().expect("run looklab-server")
}

fn stdout(output: Output) -> String {
    String::from_utf8(output.stdout).expect("stdout")
}

#[tokio::test]
async fn cli_prints_filters_and_bounds_reports() {
    let (dir, db, db_path) = database().await;
    let now = now();
    for (release, offset, page) in [
        (RELEASE_A, 30, "home"),
        (RELEASE_B, 20, "looks"),
        (RELEASE_A, 10, "look"),
    ] {
        client_errors::insert(&db, &report(release, page), now - offset)
            .await
            .expect("insert");
    }
    db.close().await;
    let config = write_config(dir.path(), &db_path);

    let json_output = cli(&config, &["--limit", "2", "--json"]);
    assert!(json_output.status.success(), "{:?}", json_output);
    let rows: Vec<Value> = serde_json::from_slice(&json_output.stdout).expect("json output");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["release"], RELEASE_A);
    assert_eq!(rows[1]["release"], RELEASE_B);
    assert_eq!(rows[0]["page"], "look");
    assert_eq!(rows[0]["kind"], "exception");
    assert_eq!(rows[0]["error_type"], "RangeError");
    assert_eq!(rows[0]["frames"][0]["asset"], "pkg/looklab-site.js");
    assert_eq!(rows[0]["frames"][0]["line"], 123);
    assert_eq!(rows[0]["frames"][0]["column"], 45);
    let timestamp = rows[0]["timestamp"].as_str().expect("timestamp");
    assert!(timestamp.ends_with('Z'));
    assert_eq!(timestamp.len(), 20);
    assert_eq!(rows[0]["received_at"], now - 10);

    let release_a = cli(&config, &["--json", "--release", RELEASE_A]);
    assert!(release_a.status.success());
    let rows: Vec<Value> = serde_json::from_slice(&release_a.stdout).expect("json output");
    assert_eq!(rows.len(), 2);

    let text = cli(&config, &["--limit", "1"]);
    assert!(text.status.success());
    let text = stdout(text);
    assert!(text.contains("release="), "{text}");
    assert!(text.contains("page=look "), "{text}");
    assert!(text.contains("kind=exception"), "{text}");
    assert!(text.contains("type=RangeError"), "{text}");
    assert!(text.contains("pkg/looklab-site.js:123:45"), "{text}");

    let too_large = cli(&config, &["--limit", "1001"]);
    assert!(!too_large.status.success());

    let bad_release = cli(&config, &["--release", "not-a-build"]);
    assert!(!bad_release.status.success());
}

#[tokio::test]
async fn cli_reads_without_migrating_the_database() {
    let dir = tempfile::tempdir().expect("temp dir");
    let db_path = dir.path().join("looklab.sqlite3");
    std::fs::File::create(&db_path).expect("empty database");
    let config = write_config(dir.path(), &db_path);
    let output = cli(&config, &["--limit", "5"]);
    assert!(!output.status.success());
    assert_eq!(
        std::fs::metadata(&db_path).expect("metadata").len(),
        0,
        "the read must not create the schema"
    );
}
