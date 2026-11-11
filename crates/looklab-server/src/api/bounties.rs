// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use crate::model_store;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use looklab_schema::{
    BodyRealizationId, BodyStateVersion, ControlEpochId, LookId, ModelReportId, ModelSubject,
    ModelVersionId, OutputEncoding, ProjectionUse, PublishedTargetId, RealizationRevisionId,
    SelectionScope, TargetBasis, TargetEligibilityStatus, TargetVersion,
};
use looklab_trust::{
    Bounty, BountyReason, BountyStatus, EpochOrdinal, ModelVersions, OutputContext, OutputPin,
    RecipeKey, RecipeScore, RecipeSource, ScoringMetric, VersionId,
};
use looklab_upload::{BodyId, ControlSettings, LookSlug, SimKey};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::rate_limit::RateKey;
use crate::state::AppState;

fn public_read(state: &AppState) -> ApiResult<()> {
    if state.rate_limiter.check(&RateKey::PublicRead) {
        Ok(())
    } else {
        Err(ApiError::new(ErrorCode::RateLimited))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    look: Option<LookSlug>,
    body: Option<BodyId>,
    limit: Option<usize>,
}

fn parse_query(uri: &Uri) -> ApiResult<Filter> {
    let Query(mut filter) = Query::<Filter>::try_from_uri(uri)
        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    filter.limit = filter.limit.map(|limit| limit.clamp(1, 100));
    Ok(filter)
}

fn includes(filter: &Filter, look: &LookSlug, body: &BodyId) -> bool {
    filter.look.as_ref().is_none_or(|value| value == look)
        && filter.body.as_ref().is_none_or(|value| value == body)
}

async fn current_recipe_authority(
    conn: &mut SqliteConnection,
    body: &BodyId,
    epoch: i64,
    base_sim: &str,
    target_look: &LookSlug,
    look_version: i64,
    source: &RecipeSource,
) -> ApiResult<bool> {
    let epoch = u32::try_from(epoch).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let version = u32::try_from(look_version).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let subject = ModelSubject::BodyRealization {
        realization: BodyRealizationId {
            body: looklab_schema::BodyId::new(body.as_str())
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            epoch: ControlEpochId::new(epoch),
            native_sim: looklab_schema::SimKey::new(base_sim)
                .map_err(|_| ApiError::new(ErrorCode::Internal))?,
        },
    }
    .key();
    let look = LookId::new(target_look.as_str()).map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let published = PublishedTargetId {
        look: look.clone(),
        version: TargetVersion::new(version).ok_or_else(|| ApiError::new(ErrorCode::Internal))?,
    };
    let target_current = match model_store::family_head(&mut *conn, &look)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        Some(head) => head.applicability.is_current() && head.target.version.get() == version,
        None => model_store::target_eligibility(&mut *conn, &published)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?
            .is_some_and(|eligibility| {
                eligibility.status == TargetEligibilityStatus::Eligible
                    && eligibility.basis == TargetBasis::LutComposition
            }),
    };
    if !target_current {
        return Ok(false);
    }
    let use_ = match source.scope {
        SelectionScope::Qualified => ProjectionUse::QualifiedMatch,
        SelectionScope::Experimental => ProjectionUse::ExperimentalMatch,
        SelectionScope::Unassessed => return Ok(false),
    };
    let projections = model_store::current_projections(&mut *conn, &subject, use_)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let selected = projections.iter().any(|projection| {
        projection.applicability.is_current()
            && projection.selected_report == source.report
            && matches!(
                &projection.selected_candidate,
                Some(ModelVersionId::BodyRealization { realization })
                    if RealizationRevisionId::for_version(realization) == source.realization
            )
    });
    if !selected {
        return Ok(false);
    }
    if let Some(report) = source.report
        && !model_store::report_applicable(&mut *conn, &report)
            .await
            .map_err(|_| ApiError::new(ErrorCode::Internal))?
    {
        return Ok(false);
    }
    let revision = model_store::realization_revision(&mut *conn, &source.realization)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(revision.is_some_and(|revision| revision.body_state_version == source.body_state))
}

fn parse_output_context(row: &sqlx::sqlite::SqliteRow) -> ApiResult<Option<OutputContext>> {
    let target_encoding: Option<String> = row.get("target_encoding");
    let target_clip: Option<f64> = row.get("target_clip");
    let output_encoding: Option<String> = row.get("output_encoding");
    let realization_clip: Option<f64> = row.get("realization_clip");
    let (Some(target_encoding), Some(target_clip), Some(output_encoding), Some(realization_clip)) = (
        target_encoding,
        target_clip,
        output_encoding,
        realization_clip,
    ) else {
        return Ok(None);
    };
    let target = OutputPin::new(
        OutputEncoding::parse(&target_encoding).map_err(|_| ApiError::new(ErrorCode::Internal))?,
        target_clip,
    )
    .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let camera = OutputPin::new(
        OutputEncoding::parse(&output_encoding).map_err(|_| ApiError::new(ErrorCode::Internal))?,
        realization_clip,
    )
    .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    Ok(Some(OutputContext { target, camera }))
}

/// `GET /api/recipes`: recipe leaderboards per Look × body.
pub async fn recipes(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    public_read(&state)?;
    let filter = parse_query(&uri)?;
    let rows = sqlx::query(
        "SELECT body_id, epoch, base_sim, settings, target_look, metric, predicted_de,
                predicted_sigma, measured_de, measured_sigma, raw_gap, signed_residual,
                n_verifications, input_transform_version, control_response_version,
                look_version, target_encoding, target_clip, output_encoding,
                realization_clip, source_realization, source_body_state, source_report,
                source_scope
         FROM recipe_scores
         WHERE source_realization IS NOT NULL AND source_body_state IS NOT NULL
           AND source_scope IS NOT NULL AND metric = ?
         ORDER BY target_look, body_id, epoch, base_sim, settings",
    )
    .bind(looklab_trust::scoring_metric_name(
        ScoringMetric::RenderedPostClipV1,
    ))
    .fetch_all(&state.db)
    .await?;

    let mut conn = state
        .db
        .acquire()
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let mut scores = Vec::with_capacity(rows.len());
    for row in &rows {
        let body: String = row.get("body_id");
        let body = BodyId::new(&body).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let look: String = row.get("target_look");
        let look = LookSlug::try_from(look).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        if !includes(&filter, &look, &body) {
            continue;
        }
        let epoch: i64 = row.get("epoch");
        let base_sim: String = row.get("base_sim");
        let look_version: i64 = row.get("look_version");
        let input_transform: i64 = row.get("input_transform_version");
        let control_response: i64 = row.get("control_response_version");
        let n_verifications: i64 = row.get("n_verifications");
        let source_realization: Option<String> = row.get("source_realization");
        let source_body_state: Option<i64> = row.get("source_body_state");
        let source_report: Option<String> = row.get("source_report");
        let source_scope: Option<String> = row.get("source_scope");
        let source = match (source_realization, source_body_state, source_scope) {
            (Some(realization), Some(body_state), Some(scope)) => Some(RecipeSource {
                realization: RealizationRevisionId::new(realization)
                    .map_err(|_| ApiError::new(ErrorCode::Internal))?,
                body_state: BodyStateVersion::new(
                    u32::try_from(body_state).map_err(|_| ApiError::new(ErrorCode::Internal))?,
                )
                .ok_or_else(|| ApiError::new(ErrorCode::Internal))?,
                report: source_report
                    .map(|report| ModelReportId::parse(&report))
                    .transpose()
                    .map_err(|_| ApiError::new(ErrorCode::Internal))?,
                scope: SelectionScope::parse(&scope)
                    .map_err(|_| ApiError::new(ErrorCode::Internal))?,
            }),
            _ => None,
        };
        let Some(source) = source else {
            continue;
        };
        if !current_recipe_authority(
            &mut conn,
            &body,
            epoch,
            &base_sim,
            &look,
            look_version,
            &source,
        )
        .await?
        {
            continue;
        }
        let settings: ControlSettings = serde_json::from_str(&row.get::<String, _>("settings"))
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let metric_text: String = row.get("metric");
        let metric = looklab_trust::parse_scoring_metric(&metric_text)
            .ok_or_else(|| ApiError::new(ErrorCode::Internal))?;
        let output = parse_output_context(row)?;
        let signed_residual: Option<[f64; 3]> = row
            .get::<Option<String>, _>("signed_residual")
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        scores.push(RecipeScore {
            metric,
            output,
            recipe: RecipeKey {
                body,
                epoch: EpochOrdinal::new(
                    u32::try_from(epoch).map_err(|_| ApiError::new(ErrorCode::Internal))?,
                ),
                base_sim: SimKey::new(&base_sim).map_err(|_| ApiError::new(ErrorCode::Internal))?,
                settings,
                target_look: look,
            },
            predicted_de: row.get("predicted_de"),
            predicted_sigma: row.get("predicted_sigma"),
            measured_de: row.get("measured_de"),
            measured_sigma: row.get("measured_sigma"),
            raw_gap: row.get("raw_gap"),
            signed_residual,
            n_verifications: usize::try_from(n_verifications).unwrap_or(usize::MAX),
            model_versions: ModelVersions {
                input_transform: u32::try_from(input_transform).unwrap_or(0),
                control_response: u32::try_from(control_response).unwrap_or(0),
                look: VersionId::new(u32::try_from(look_version).unwrap_or(0)),
            },
            source: Some(source),
        });
    }
    Ok(Json(looklab_trust::leaderboards(scores)).into_response())
}

#[derive(Serialize)]
struct BountyList {
    bounties: Vec<Bounty>,
    evidence_requests: Vec<looklab_schema::EvidenceRequestWire>,
}

fn parse_status(value: &str) -> Option<BountyStatus> {
    match value {
        "open" => Some(BountyStatus::Open),
        "resolved" => Some(BountyStatus::Resolved),
        _ => None,
    }
}

/// `GET /bounties.json`: published open bounties, highest expected reduction first.
pub async fn bounties(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    public_read(&state)?;
    let filter = parse_query(&uri)?;
    let limit = i64::try_from(filter.limit.unwrap_or(20)).unwrap_or(20);
    let rows = sqlx::query(
        r#"SELECT target_look, look_version, body_id, region, target_encoding, target_clip,
                output_encoding, realization_clip, expected_variance_reduction, reason, status
         FROM bounties
         WHERE status = 'open'
           AND (
             metric = ?
             OR NOT (
               json_valid(reason)
               AND json_extract(reason, '$.kind') = 'persistent_residual'
             )
           )
         ORDER BY expected_variance_reduction DESC, target_look, body_id, region
         LIMIT ?"#,
    )
    .bind(looklab_trust::scoring_metric_name(
        ScoringMetric::RenderedPostClipV1,
    ))
    .bind(limit)
    .fetch_all(&state.db)
    .await?;

    let mut bounties = Vec::with_capacity(rows.len());
    for row in &rows {
        let look: String = row.get("target_look");
        let look = LookSlug::try_from(look).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let body: Option<String> = row.get("body_id");
        let body = match body {
            Some(value) => {
                Some(BodyId::new(&value).map_err(|_| ApiError::new(ErrorCode::Internal))?)
            }
            None => None,
        };
        if !filter.look.as_ref().is_none_or(|value| value == &look) {
            continue;
        }
        if let Some(wanted) = &filter.body
            && body.as_ref() != Some(wanted)
        {
            continue;
        }
        let region: String = row.get("region");
        let region = region
            .parse()
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let reason: String = row.get("reason");
        let reason: BountyReason =
            serde_json::from_str(&reason).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let status: String = row.get("status");
        let status = parse_status(&status).ok_or_else(|| ApiError::new(ErrorCode::Internal))?;
        let look_version: i64 = row.get("look_version");
        bounties.push(Bounty {
            look,
            look_version: VersionId::new(u32::try_from(look_version).unwrap_or(0)),
            body,
            region,
            output: parse_output_context(row)?,
            expected_variance_reduction: row.get("expected_variance_reduction"),
            reason,
            status,
        });
    }
    let mut conn = state.db.acquire().await?;
    let evidence_rows = crate::model_store::evidence_requests(&mut conn)
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?;
    drop(conn);
    let mut evidence_requests = Vec::with_capacity(evidence_rows.len());
    for stored in &evidence_rows {
        let parsed = &stored.subject;
        match parsed {
            looklab_schema::ModelSubject::BodyRealization { realization } => {
                if let Some(wanted) = &filter.body
                    && realization.body.as_str() != wanted.as_str()
                {
                    continue;
                }
                if filter.look.is_some() {
                    continue;
                }
            }
            looklab_schema::ModelSubject::SharedFamily { family } => {
                if let Some(wanted) = &filter.look
                    && family.as_str() != wanted.as_str()
                {
                    continue;
                }
                if filter.body.is_some() {
                    continue;
                }
            }
        }
        let origin = match &stored.origin {
            crate::model_store::EvidenceRequestOrigin::Report(report) => {
                looklab_schema::EvidenceRequestOrigin::Report { report: *report }
            }
            crate::model_store::EvidenceRequestOrigin::PreCandidate => {
                looklab_schema::EvidenceRequestOrigin::PreCandidate
            }
        };
        evidence_requests.push(looklab_schema::EvidenceRequestWire {
            origin,
            subject: parsed.clone(),
            capability: stored.capability,
            kind: stored.request.kind,
            detail: stored.request.detail.clone(),
            observed_groups: stored.observed_groups,
        });
    }
    Ok(Json(BountyList {
        bounties,
        evidence_requests,
    })
    .into_response())
}
