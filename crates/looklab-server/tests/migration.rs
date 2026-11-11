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
async fn lut_upload_tables_and_blob_references_exist_after_migration() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("memory database");
    sqlx::migrate!("./migrations")
        .run(&pool)
        .await
        .expect("migrations");
    for table in [
        "lut_uploads",
        "lut_imports",
        "lut_import_failures",
        "lut_blob_deletions",
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
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES ('lut-owner', x'03', '{}', 0)",
    )
    .execute(&pool)
    .await
    .expect("credential");
    sqlx::query(
        "INSERT INTO proposals
         (id, credential_id, type, payload, human_summary, evidence, status, created_at)
         VALUES ('lut-proposal', 'lut-owner', 'lut_look', '{}', 'x', '[]', 'pending', 0)",
    )
    .execute(&pool)
    .await
    .expect("proposal");
    let hash = [0xabu8; 32];
    sqlx::query(
        "INSERT INTO lut_uploads (proposal_id, sha256, credential_id, file, size, created_at)
         VALUES ('lut-proposal', ?, 'lut-owner', 'my-lut.cube', 4096, 0)",
    )
    .bind(hash.as_slice())
    .execute(&pool)
    .await
    .expect("upload");
    let referenced: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blob_references WHERE hash = ?")
        .bind("ab".repeat(32))
        .fetch_one(&pool)
        .await
        .expect("references");
    assert_eq!(referenced, 1);
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

async fn moderation_pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .expect("memory database");
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&pool)
        .await
        .expect("foreign keys");
    sqlx::raw_sql(include_str!("../migrations/0001_init.sql"))
        .execute(&pool)
        .await
        .expect("initial schema");
    sqlx::raw_sql(include_str!(
        "../migrations/0035_contribution_moderation.sql"
    ))
    .execute(&pool)
    .await
    .expect("moderation schema");
    sqlx::query(
        "INSERT INTO credentials (id, credential_id, webauthn_credential, created_at)
         VALUES ('subject', x'01', '{}', 0)",
    )
    .execute(&pool)
    .await
    .expect("credential");
    sqlx::query(
        "INSERT INTO contributions
         (id, credential_id, batch_manifest_hash, signature, pow, kind, intent, body_id,
          firmware_version, sim_key, settings, status, created_at)
         VALUES ('contribution', 'subject', x'00', x'00', 'pow', 'library', 'native',
                 'fujifilm/x-t5', '1.0', 'sim', '[]', 'accepted', 0)",
    )
    .execute(&pool)
    .await
    .expect("contribution");
    sqlx::query(
        "INSERT INTO contribution_images
         (id, contribution_id, samples_hash, n_samples, meta, status)
         VALUES ('image', 'contribution', x'00', 1, '{}', 'accepted')",
    )
    .execute(&pool)
    .await
    .expect("image");
    pool
}

#[tokio::test]
async fn contribution_moderation_state_is_append_only_and_sticky() {
    let pool = moderation_pool().await;
    sqlx::query(
        "INSERT INTO contribution_rejections
         (contribution_id, moderator_credential, reason, rejected_at)
         VALUES ('contribution', 'cli', 'bad', 1)",
    )
    .execute(&pool)
    .await
    .expect("rejection");
    sqlx::query(
        "INSERT INTO credential_intake_blocks
         (credential_id, moderator_credential, reason, blocked_at)
         VALUES ('subject', 'cli', 'bad', 1)",
    )
    .execute(&pool)
    .await
    .expect("block");
    sqlx::query("UPDATE contributions SET status = 'quarantined' WHERE id = 'contribution'")
        .execute(&pool)
        .await
        .expect("quarantine contribution");
    sqlx::query("UPDATE contribution_images SET status = 'quarantined' WHERE id = 'image'")
        .execute(&pool)
        .await
        .expect("quarantine image");

    sqlx::query("UPDATE contributions SET status = 'accepted' WHERE id = 'contribution'")
        .execute(&pool)
        .await
        .expect("automated contribution update");
    sqlx::query("UPDATE contribution_images SET status = 'accepted' WHERE id = 'image'")
        .execute(&pool)
        .await
        .expect("automated image update");
    let status: String =
        sqlx::query_scalar("SELECT status FROM contributions WHERE id = 'contribution'")
            .fetch_one(&pool)
            .await
            .expect("contribution status");
    assert_eq!(status, "quarantined");
    let image: String =
        sqlx::query_scalar("SELECT status FROM contribution_images WHERE id = 'image'")
            .fetch_one(&pool)
            .await
            .expect("image status");
    assert_eq!(image, "quarantined");

    assert!(
        sqlx::query("UPDATE contribution_rejections SET reason = 'changed'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM contribution_rejections")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE credential_intake_blocks SET reason = 'changed'")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM credential_intake_blocks")
            .execute(&pool)
            .await
            .is_err()
    );

    sqlx::query("UPDATE contributions SET status = 'withdrawn' WHERE id = 'contribution'")
        .execute(&pool)
        .await
        .expect("withdrawal remains allowed");
}
