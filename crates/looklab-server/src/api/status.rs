// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{CredentialId, StatusToken};
use crate::rate_limit::Action;
use crate::state::AppState;
use crate::store::{self, ActivityFeed, ContributionStatus};
use crate::trust::Tier;
use crate::verify::{RequestDomain, SignedRequestPayload};

#[derive(Serialize)]
pub struct StatusResponse {
    credential: CredentialId,
    activities: Vec<ActivityFeed>,
}

async fn feed(state: &AppState, token: &str) -> ApiResult<(CredentialId, Vec<ActivityFeed>)> {
    let token =
        StatusToken::from_base64url(token).ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    let credential = store::credential_id_for_token(&state.db, &token.hash())
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    let activities = store::status_feed(&state.db, &credential).await?;
    Ok((credential, activities))
}

pub async fn status(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    if let Some(token) = token.strip_suffix(".rss") {
        return rss(&state, token).await;
    }
    let (credential, activities) = feed(&state, &token).await?;
    Ok(Json(StatusResponse {
        credential,
        activities,
    })
    .into_response())
}

async fn rss(state: &AppState, token: &str) -> ApiResult<Response> {
    let (credential, activities) = feed(state, token).await?;
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\"><channel>");
    xml.push_str("<title>looklab contribution status</title>");
    let _ = write!(
        xml,
        "<link>looklab-status:{}</link>",
        escape_xml(credential.as_str())
    );
    xml.push_str("<description>Status of your contributions.</description>");
    for activity in activities {
        xml.push_str("<item>");
        let _ = write!(
            xml,
            "<title>{}</title>",
            escape_xml(&format!(
                "Contribution from {}",
                format_day(activity.created_at)
            ))
        );
        let photos: usize = activity
            .contributions
            .iter()
            .map(|contribution| contribution.images.len())
            .sum();
        let mut description = format!("{photos} photos\n");
        let mut photo_index = 0;
        for contribution in &activity.contributions {
            if contribution.status == ContributionStatus::Withdrawn {
                description.push_str("withdrawn\n");
                photo_index += contribution.images.len();
                continue;
            }
            if contribution.render_class == looklab_upload::RenderClass::Monochrome
                && contribution.status == ContributionStatus::Pending
            {
                description.push_str("received, awaiting monochrome analysis\n");
                photo_index += contribution.images.len();
                continue;
            }
            for image in &contribution.images {
                photo_index += 1;
                let _ = writeln!(
                    description,
                    "Photo {photo_index}: {}{}",
                    image.status,
                    image
                        .reason
                        .as_deref()
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default()
                );
            }
        }
        let _ = write!(
            xml,
            "<description>{}</description><guid isPermaLink=\"false\">{}</guid>",
            escape_xml(&description),
            activity.identity
        );
        xml.push_str("</item>");
    }
    xml.push_str("</channel></rss>");
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/rss+xml; charset=utf-8")],
        xml,
    )
        .into_response())
}

fn format_day(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + if month <= 2 { 1 } else { 0 };
    format!("{year:04}-{month:02}-{day:02}")
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

#[derive(Serialize)]
pub struct SessionResponse {
    status_token: String,
    roles: Vec<looklab_trust::Role>,
    tier: Tier,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_handle: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct SessionRequest {
    #[serde(flatten)]
    signed: SignedRequestPayload,
    pow: Option<[u64; looklab_upload::POW_K]>,
}

pub async fn session(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SessionRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let credential = super::auth::authenticate_with_pow(
        &state,
        &payload.signed,
        RequestDomain::Session,
        &[],
        Action::Session,
        payload.pow.map(looklab_upload::PowSolutions::new),
    )
    .await?;
    let token = StatusToken::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let mut tx = state.db.begin().await?;
    store::insert_status_token(&mut tx, &credential, &token.hash()).await?;
    tx.commit().await?;
    session_response(&state, &credential, &token).await
}

pub async fn restore_session(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let token = super::auth::bearer_token(&headers)?;
    let credential = store::credential_id_for_token(&state.db, &token.hash())
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    session_response(&state, &credential, &token).await
}

async fn session_response(
    state: &AppState,
    credential: &CredentialId,
    token: &StatusToken,
) -> ApiResult<Response> {
    let standing = super::auth::roles(state, credential).await?;
    let display_handle: Option<String> =
        sqlx::query_scalar("SELECT display_handle FROM credentials WHERE id = ?")
            .bind(credential.as_str())
            .fetch_one(&state.db)
            .await?;
    Ok(Json(SessionResponse {
        status_token: token.as_str().to_owned(),
        roles: standing.roles,
        tier: standing.tier,
        display_handle,
    })
    .into_response())
}

pub async fn revoke_session(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SignedRequestPayload>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let credential = super::auth::authenticate(
        &state,
        &payload,
        RequestDomain::SessionRevoke,
        &[],
        Action::SessionRevoke,
    )
    .await?;
    let mut tx = state.db.begin().await?;
    store::revoke_tokens(&mut tx, &credential).await?;
    tx.commit().await?;
    state.mod_sessions.close_credential(&credential);
    Ok(StatusCode::NO_CONTENT.into_response())
}
