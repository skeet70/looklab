// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_trust::{Scope, ScopeRule};
use looklab_upload::{BodyId, LookSlug};
use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

async fn legacy_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("memory database");
    sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
        .execute(&pool)
        .await
        .expect("initial schema");
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES ('subject', x'01', '{}', 0), ('issuer', x'02', '{}', 0)",
    )
    .execute(&pool)
    .await
    .expect("credentials");
    let grant = Scope::new([ScopeRule::body_all_looks(
        BodyId::new("fujifilm/x-t5").expect("body"),
    )]);
    let rule_text = serde_json::to_string(&grant).expect("scope");
    for (id, scope) in [
        (
            "legacy-grant",
            r#"{"bodies":["fujifilm/x-t5"],"looks":["classic-chrome"]}"#,
        ),
        ("rule-grant", rule_text.as_str()),
    ] {
        sqlx::query(
            "INSERT INTO grants
             (id, subject_credential, role_or_tier, scope, granted_by, reason, granted_at)
             VALUES (?, 'subject', 'seeder', ?, 'issuer', 'legacy', 0)",
        )
        .bind(id)
        .bind(scope)
        .execute(&pool)
        .await
        .expect("legacy grant");
    }
    sqlx::query(
        "INSERT INTO endorsements
         (id, issuer_credential, subject_credential, scope, reason, issued_at)
         VALUES ('endorsement', 'issuer', 'subject',
                 '{\"bodies\":[\"fujifilm/x-t5\"],\"looks\":[\"classic-chrome\"]}', 'legacy', 0)",
    )
    .execute(&pool)
    .await
    .expect("legacy endorsement");
    sqlx::raw_sql(include_str!("../migrations/0002_scope_rules.sql"))
        .execute(&pool)
        .await
        .expect("scope migration");
    pool
}

#[tokio::test]
async fn legacy_or_scope_migrates_to_rule_lists() {
    let pool = legacy_pool().await;
    let legacy: String = sqlx::query_scalar("SELECT scope FROM grants WHERE id = 'legacy-grant'")
        .fetch_one(&pool)
        .await
        .expect("legacy grant");
    let legacy: Scope = serde_json::from_str(&legacy).expect("legacy scope");
    let body = BodyId::new("fujifilm/x-t5").expect("body");
    let look = LookSlug::try_from("classic-chrome").expect("look");
    assert!(
        legacy
            .rules
            .contains(&ScopeRule::body_all_looks(body.clone()))
    );
    assert!(legacy.rules.contains(&ScopeRule::any_body(look.clone())));
    let rules: String = sqlx::query_scalar("SELECT scope FROM grants WHERE id = 'rule-grant'")
        .fetch_one(&pool)
        .await
        .expect("rule grant");
    let rules: Scope = serde_json::from_str(&rules).expect("rule scope");
    assert_eq!(rules.rules, vec![ScopeRule::body_all_looks(body)]);
    let endorsement: String =
        sqlx::query_scalar("SELECT scope FROM endorsements WHERE id = 'endorsement'")
            .fetch_one(&pool)
            .await
            .expect("endorsement");
    let endorsement: Scope = serde_json::from_str(&endorsement).expect("endorsement scope");
    assert_eq!(endorsement.rules.len(), 2);
}

#[tokio::test]
async fn bounty_tables_exist_after_migration() {
    let pool = legacy_pool().await;
    sqlx::raw_sql(include_str!("../migrations/0008_bounties.sql"))
        .execute(&pool)
        .await
        .expect("bounty migration");
    for table in [
        "recipe_observations",
        "recipe_scores",
        "recipe_residuals",
        "bounties",
    ] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("table count");
        assert_eq!(count, 1, "missing table {table}");
    }
}

#[tokio::test]
async fn new_manifest_tables_exist_after_migration() {
    let pool = legacy_pool().await;
    for table in [
        "look_versions",
        "version_manifests",
        "version_depends_on_root",
        "version_regating",
        "gate_failures",
    ] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        )
        .bind(table)
        .fetch_one(&pool)
        .await
        .expect("table count");
        assert_eq!(count, 1, "missing table {table}");
    }
}
