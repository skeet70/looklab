// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::Uri;
use axum::response::{IntoResponse, Response};
use looklab_trust::{
    Bounty, BountyReason, BountyStatus, EpochOrdinal, ModelVersions, RecipeKey, RecipeScore,
    VersionId,
};
use looklab_upload::{BodyId, ControlSettings, LookSlug, SimKey};
use serde::Serialize;
use sqlx::Row;

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

#[derive(Debug, Default)]
struct Filter {
    look: Option<LookSlug>,
    body: Option<BodyId>,
    limit: Option<usize>,
}

fn parse_query(uri: &Uri) -> ApiResult<Filter> {
    let mut filter = Filter::default();
    let Some(raw) = uri.query() else {
        return Ok(filter);
    };
    for pair in raw.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| ApiError::new(ErrorCode::MalformedEnvelope))?;
        match key {
            "look" if !value.is_empty() => {
                filter.look = Some(
                    LookSlug::try_from(value)
                        .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?,
                );
            }
            "body" if !value.is_empty() => {
                filter.body = Some(
                    BodyId::new(value).map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?,
                );
            }
            "limit" => {
                let limit: usize = value
                    .parse()
                    .map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
                filter.limit = Some(limit.clamp(1, 100));
            }
            _ => return Err(ApiError::new(ErrorCode::MalformedEnvelope)),
        }
    }
    Ok(filter)
}

fn includes(filter: &Filter, look: &LookSlug, body: &BodyId) -> bool {
    filter.look.as_ref().is_none_or(|value| value == look)
        && filter.body.as_ref().is_none_or(|value| value == body)
}

/// `GET /api/recipes`: recipe leaderboards per Look × body.
pub async fn recipes(State(state): State<Arc<AppState>>, uri: Uri) -> ApiResult<Response> {
    public_read(&state)?;
    let filter = parse_query(&uri)?;
    let rows = sqlx::query(
        "SELECT body_id, epoch, base_sim, settings, target_look, predicted_de,
                predicted_sigma, measured_de, measured_sigma, n_verifications,
                input_transform_version, control_response_version, look_version
         FROM recipe_scores ORDER BY target_look, body_id, epoch, base_sim, settings",
    )
    .fetch_all(&state.db)
    .await?;

    let mut scores = Vec::with_capacity(rows.len());
    for row in &rows {
        let body: String = row.get("body_id");
        let body = BodyId::new(&body).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let look: String = row.get("target_look");
        let look = LookSlug::try_from(look).map_err(|_| ApiError::new(ErrorCode::Internal))?;
        if !includes(&filter, &look, &body) {
            continue;
        }
        let settings: ControlSettings = serde_json::from_str(&row.get::<String, _>("settings"))
            .map_err(|_| ApiError::new(ErrorCode::Internal))?;
        let epoch: i64 = row.get("epoch");
        let base_sim: String = row.get("base_sim");
        let look_version: i64 = row.get("look_version");
        let input_transform: i64 = row.get("input_transform_version");
        let control_response: i64 = row.get("control_response_version");
        let n_verifications: i64 = row.get("n_verifications");
        scores.push(RecipeScore {
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
            n_verifications: usize::try_from(n_verifications).unwrap_or(usize::MAX),
            model_versions: ModelVersions {
                input_transform: u32::try_from(input_transform).unwrap_or(0),
                control_response: u32::try_from(control_response).unwrap_or(0),
                look: VersionId::new(u32::try_from(look_version).unwrap_or(0)),
            },
        });
    }
    Ok(Json(looklab_trust::leaderboards(scores)).into_response())
}

#[derive(Serialize)]
struct BountyList {
    bounties: Vec<Bounty>,
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
        "SELECT target_look, look_version, body_id, region, expected_variance_reduction,
                reason, status
         FROM bounties WHERE status = 'open'
         ORDER BY expected_variance_reduction DESC, target_look, body_id, region
         LIMIT ?",
    )
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
            expected_variance_reduction: row.get("expected_variance_reduction"),
            reason,
            status,
        });
    }
    Ok(Json(BountyList { bounties }).into_response())
}
