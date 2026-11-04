// SPDX-License-Identifier: AGPL-3.0-or-later

use std::fmt::Write as _;
use std::sync::Arc;

use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use looklab_upload::Challenge;
use serde::Serialize;

use crate::api::consume_nonce;
use crate::error::{ApiError, ApiResult, ErrorCode};
use crate::ids::{CredentialId, RawCredentialId, StatusToken};
use crate::rate_limit::{Action, CREDENTIAL_SIGNED_QUOTA, RateKey};
use crate::state::AppState;
use crate::store::{self, ContributionFeed};
use crate::verify::{RequestDomain, SignedRequestPayload, request_digest, verify_assertion};

#[derive(Serialize)]
pub struct StatusResponse {
    credential: CredentialId,
    contributions: Vec<ContributionFeed>,
}

async fn feed(state: &AppState, token: &str) -> ApiResult<(CredentialId, Vec<ContributionFeed>)> {
    let token =
        StatusToken::from_base64url(token).ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    let credential = store::credential_id_for_token(&state.db, &token.hash())
        .await?
        .ok_or_else(|| ApiError::new(ErrorCode::TokenUnknown))?;
    let contributions = store::status_feed(&state.db, &credential).await?;
    Ok((credential, contributions))
}

pub async fn status(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> ApiResult<Response> {
    if let Some(token) = token.strip_suffix(".rss") {
        return rss(&state, token).await;
    }
    let (credential, contributions) = feed(&state, &token).await?;
    Ok(Json(StatusResponse {
        credential,
        contributions,
    })
    .into_response())
}

async fn rss(state: &AppState, token: &str) -> ApiResult<Response> {
    let (credential, contributions) = feed(state, token).await?;
    let mut xml =
        String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<rss version=\"2.0\"><channel>");
    xml.push_str("<title>looklab contribution status</title>");
    let _ = write!(
        xml,
        "<link>looklab-status:{}</link>",
        escape_xml(credential.as_str())
    );
    xml.push_str("<description>Per-image status of your contributions.</description>");
    for contribution in contributions {
        xml.push_str("<item>");
        let _ = write!(
            xml,
            "<title>{}</title>",
            escape_xml(&format!("Contribution {}", contribution.id))
        );
        let mut description = String::new();
        for image in &contribution.images {
            let _ = writeln!(
                description,
                "{}: {}{}",
                image.id,
                image.status,
                image
                    .reason
                    .as_deref()
                    .map(|reason| format!(" ({reason})"))
                    .unwrap_or_default()
            );
        }
        let _ = write!(
            xml,
            "<description>{}</description><guid isPermaLink=\"false\">{}</guid>",
            escape_xml(&description),
            escape_xml(contribution.id.as_str())
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
pub struct ReissueResponse {
    status_token: String,
}

pub async fn reissue(
    State(state): State<Arc<AppState>>,
    payload: Result<Json<SignedRequestPayload>, JsonRejection>,
) -> ApiResult<Response> {
    let Json(payload) = payload.map_err(|_| ApiError::new(ErrorCode::MalformedEnvelope))?;
    let (assertion, nonce) = payload.decode()?;
    let raw_id = RawCredentialId::new(assertion.credential_id.clone());
    if !state.rate_limiter.check(
        &RateKey::Assertion {
            action: Action::StatusReissue,
            id: raw_id.clone(),
        },
        CREDENTIAL_SIGNED_QUOTA,
    ) {
        return Err(ApiError::new(ErrorCode::RateLimited));
    }
    consume_nonce(&state, &nonce).await?;

    let credential = store::find_credential(&state.db, raw_id.as_slice())
        .await
        .map_err(|_| ApiError::new(ErrorCode::Internal))?
        .ok_or_else(|| ApiError::new(ErrorCode::CredentialUnknown))?;
    let challenge = Challenge::new(&nonce, &request_digest(RequestDomain::StatusReissue, &[]));
    verify_assertion(
        &credential.passkey,
        &assertion,
        challenge,
        &state.config.origin,
        &state.config.rp_id,
    )
    .map_err(|_| ApiError::new(ErrorCode::SignatureInvalid))?;

    let token = StatusToken::generate().map_err(|_| ApiError::new(ErrorCode::Internal))?;
    let mut tx = state.db.begin().await?;
    store::revoke_tokens(&mut tx, &credential.id).await?;
    store::insert_status_token(&mut tx, &credential.id, &token.hash()).await?;
    tx.commit().await?;

    Ok(Json(ReissueResponse {
        status_token: token.as_str().to_owned(),
    })
    .into_response())
}
