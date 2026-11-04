// SPDX-License-Identifier: AGPL-3.0-or-later

use assert_cmd::Command;
use looklab_server::CredentialId;
use looklab_trust::{Scope, ScopeRule};
use looklab_upload::BodyId;
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
