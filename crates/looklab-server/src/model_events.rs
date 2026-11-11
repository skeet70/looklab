// SPDX-License-Identifier: AGPL-3.0-or-later

use looklab_schema::{
    BodyId, ContributionRef, DependencyKind, DependencyRef, DependencyTarget,
    IdentityResolutionRef, InvalidationReason, LookId, RootAdmissionRef, RootWeightingRef,
    StandingRef, SubjectKey,
};
use looklab_trust::Scope;
use serde::Serialize;
use sqlx::SqliteConnection;

use crate::ids::{ContributionId, CredentialId, RootAdmissionId};
use crate::{model_store, store};

fn database_error(error: impl core::fmt::Display) -> sqlx::Error {
    sqlx::Error::Protocol(error.to_string())
}

async fn invalidate(
    conn: &mut SqliteConnection,
    dependency: DependencyRef,
    reason: InvalidationReason,
    now: i64,
) -> Result<(), sqlx::Error> {
    let affected = model_store::invalidate(conn, &dependency, reason, now)
        .await
        .map_err(database_error)?;
    finish_invalidation(conn, affected, now).await
}

async fn finish_invalidation(
    conn: &mut SqliteConnection,
    affected: model_store::InvalidationSummary,
    now: i64,
) -> Result<(), sqlx::Error> {
    if affected.projections != 0
        || affected.family_heads != 0
        || affected.target_eligibilities != 0
        || affected.bundles != 0
        || affected.reports != 0
    {
        store::request_publication(conn).await?;
    }
    request_refit(conn, now).await
}

#[derive(Serialize)]
struct NativeRefitPayload<'a> {
    bodies: &'a [BodyId],
}

async fn request_refit(conn: &mut SqliteConnection, now: i64) -> Result<(), sqlx::Error> {
    let queued: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE kind = ? AND status = 'queued')",
    )
    .bind(store::JobKind::NightlyRefit.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if !queued {
        let payload =
            serde_json::to_string(&NativeRefitPayload { bodies: &[] }).map_err(database_error)?;
        store::enqueue_job(conn, store::JobKind::NightlyRefit, &payload, now).await?;
    }
    Ok(())
}

async fn contribution_changed(
    conn: &mut SqliteConnection,
    contribution: &ContributionId,
    reason: InvalidationReason,
    now: i64,
) -> Result<(), sqlx::Error> {
    let id = ContributionRef::new(contribution.as_str()).map_err(database_error)?;
    invalidate(
        conn,
        DependencyRef::required(
            DependencyKind::Contribution,
            DependencyTarget::contribution(&id),
        ),
        reason,
        now,
    )
    .await
}

pub(crate) async fn contribution_withdrawn(
    conn: &mut SqliteConnection,
    contribution: &ContributionId,
    now: i64,
) -> Result<(), sqlx::Error> {
    contribution_changed(conn, contribution, InvalidationReason::Withdrawal, now).await
}

pub(crate) async fn contribution_rejected(
    conn: &mut SqliteConnection,
    contribution: &ContributionId,
    now: i64,
) -> Result<(), sqlx::Error> {
    contribution_changed(conn, contribution, InvalidationReason::Exclusion, now).await
}

pub(crate) async fn root_revoked(
    conn: &mut SqliteConnection,
    root: &RootAdmissionId,
    now: i64,
) -> Result<(), sqlx::Error> {
    let id = RootAdmissionRef::new(root.as_str()).map_err(database_error)?;
    invalidate(
        conn,
        DependencyRef::required(
            DependencyKind::RootAdmission,
            DependencyTarget::root_admission(&id),
        ),
        InvalidationReason::RootRevocation,
        now,
    )
    .await
}

pub(crate) async fn standing_changed(
    conn: &mut SqliteConnection,
    credential: &CredentialId,
    now: i64,
) -> Result<(), sqlx::Error> {
    let id = StandingRef::new(credential.as_str()).map_err(database_error)?;
    invalidate(
        conn,
        DependencyRef::required(DependencyKind::Standing, DependencyTarget::standing(&id)),
        InvalidationReason::StandingReinterpretation,
        now,
    )
    .await
}

pub(crate) async fn identity_changed(
    conn: &mut SqliteConnection,
    credential: &CredentialId,
    now: i64,
) -> Result<(), sqlx::Error> {
    let id = IdentityResolutionRef::new(credential.as_str()).map_err(database_error)?;
    invalidate(
        conn,
        DependencyRef::required(
            DependencyKind::IdentityResolution,
            DependencyTarget::identity_resolution(&id),
        ),
        InvalidationReason::IdentityMerge,
        now,
    )
    .await
}

pub(crate) struct IdentityPartition {
    credential: CredentialId,
    members: std::collections::BTreeSet<CredentialId>,
}

async fn identity_members(
    conn: &mut SqliteConnection,
    credential: &CredentialId,
) -> Result<std::collections::BTreeSet<CredentialId>, sqlx::Error> {
    let members: Vec<String> = sqlx::query_scalar(
        "WITH RECURSIVE linked(credential_id) AS (
             SELECT ?
             UNION
             SELECT other.credential_id
             FROM linked
             JOIN contributions own ON own.credential_id = linked.credential_id
             JOIN contributions other ON other.serial_hmac = own.serial_hmac
             WHERE own.serial_hmac IS NOT NULL
         )
         SELECT credential_id FROM linked ORDER BY credential_id",
    )
    .bind(credential.as_str())
    .fetch_all(&mut *conn)
    .await?;
    members
        .into_iter()
        .map(|member| CredentialId::try_from(member).map_err(database_error))
        .collect()
}

pub(crate) async fn prepare_identity_link(
    conn: &mut SqliteConnection,
    contribution: &store::NewContribution,
) -> Result<Option<IdentityPartition>, sqlx::Error> {
    let Some(hmac) = contribution.serial_hmac else {
        return Ok(None);
    };
    let connects_another: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM contributions
         WHERE serial_hmac = ? AND credential_id != ?)",
    )
    .bind(hmac.as_slice())
    .bind(contribution.credential_id.as_str())
    .fetch_one(&mut *conn)
    .await?;
    if !connects_another {
        return Ok(None);
    }
    Ok(Some(IdentityPartition {
        credential: contribution.credential_id.clone(),
        members: identity_members(conn, &contribution.credential_id).await?,
    }))
}

pub(crate) async fn finish_identity_link(
    conn: &mut SqliteConnection,
    previous: Option<IdentityPartition>,
    now: i64,
) -> Result<(), sqlx::Error> {
    let Some(previous) = previous else {
        return Ok(());
    };
    let current = identity_members(conn, &previous.credential).await?;
    if previous.members != current {
        for credential in previous.members.union(&current) {
            identity_changed(conn, credential, now).await?;
        }
    }
    Ok(())
}

pub(crate) async fn root_admitted(
    conn: &mut SqliteConnection,
    scope: &Scope,
    now: i64,
) -> Result<(), sqlx::Error> {
    let mut weightings: std::collections::BTreeSet<RootWeightingRef> =
        std::collections::BTreeSet::new();
    for rule in &scope.rules {
        if let Some(body) = &rule.body {
            weightings.insert(RootWeightingRef::body(body));
        }
        if let Some(look) = &rule.look {
            let family = LookId::new(look.as_str()).map_err(database_error)?;
            weightings.insert(RootWeightingRef::family(&family));
        }
        if rule.body.is_none() && rule.look.is_none() {
            weightings.insert(RootWeightingRef::global());
        }
    }
    for weighting in weightings {
        invalidate(
            conn,
            DependencyRef::required(
                DependencyKind::RootWeighting,
                DependencyTarget::root_weighting(&weighting),
            ),
            InvalidationReason::RootWeightingChange,
            now,
        )
        .await?;
    }
    let mut subjects: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for rule in &scope.rules {
        if let Some(look) = &rule.look {
            let family = LookId::new(look.as_str()).map_err(database_error)?;
            subjects.insert(SubjectKey::for_family(&family).as_str().to_owned());
            let realizations: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT subject_key FROM model_realization_revisions WHERE family = ?",
            )
            .bind(look.as_str())
            .fetch_all(&mut *conn)
            .await?;
            subjects.extend(realizations);
        }
        if let Some(body) = &rule.body {
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT key FROM model_subjects
                 WHERE kind = 'body_realization' AND body = ?",
            )
            .bind(body.as_str())
            .fetch_all(&mut *conn)
            .await?;
            subjects.extend(keys);
        }
        if rule.body.is_none() && rule.look.is_none() {
            let keys: Vec<String> = sqlx::query_scalar("SELECT key FROM model_subjects")
                .fetch_all(&mut *conn)
                .await?;
            subjects.extend(keys);
        }
    }
    for subject in subjects {
        let subject = SubjectKey::new(subject).map_err(database_error)?;
        model_store::note_evidence_arrival(conn, &subject, now)
            .await
            .map_err(database_error)?;
    }
    request_refit(conn, now).await
}

pub(crate) async fn schema_reinterpreted(
    conn: &mut SqliteConnection,
    body: &BodyId,
    now: i64,
) -> Result<(), sqlx::Error> {
    let affected = model_store::invalidate_schema_versions(
        conn,
        body,
        &[],
        InvalidationReason::SchemaReinterpretation,
        now,
    )
    .await
    .map_err(database_error)?;
    finish_invalidation(conn, affected, now).await
}
