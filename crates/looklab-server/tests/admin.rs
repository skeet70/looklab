// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_server::admin::{self, AdminError};
use looklab_server::{ContributionId, CredentialId, EndorsementId, GrantId, LookVersionId, Tier};
use looklab_trust::{Actor, Grant, Scope, ScopeRule, TrustError, VersionId};
use looklab_upload::{BodyId, LookSlug};
use sqlx::SqlitePool;
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    db: SqlitePool,
}

async fn fixture() -> Fixture {
    let dir = TempDir::new().expect("temp dir");
    let db = looklab_server::open_database(&dir.path().join("looklab.sqlite3"))
        .await
        .expect("database");
    Fixture { _dir: dir, db }
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
async fn endorsement_nominations_are_seeder_only() {
    let fixture = fixture().await;
    let db = &fixture.db;
    let nominated = credential(db, "new", "[]", 1.4).await;

    let queue = admin::mod_queue(db, Tier::New).await.expect("queue");
    assert!(queue.nominations.is_empty());

    let queue = admin::mod_queue(db, Tier::Seeder).await.expect("queue");
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
