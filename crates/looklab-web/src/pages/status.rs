// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-image status from the capability token ([docs/06 §4](../../../docs/06-privacy-and-payload.md)).

use gloo_net::http::Request;
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use serde::Deserialize;

use crate::data::Load;
use crate::ui;

#[derive(Clone, Debug, Deserialize)]
struct StatusResponse {
    #[serde(rename = "credential")]
    _credential: String,
    contributions: Vec<ContributionView>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(transparent)]
struct ContributionId(String);

impl core::fmt::Display for ContributionId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ContributionView {
    id: ContributionId,
    status: ContributionStatus,
    created_at: i64,
    images: Vec<ImageFeedView>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(from = "String")]
enum ContributionStatus {
    Pending,
    Accepted,
    Quarantined,
    Withdrawn,
    Unknown,
}

impl From<String> for ContributionStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "pending" => Self::Pending,
            "accepted" => Self::Accepted,
            "quarantined" => Self::Quarantined,
            "withdrawn" => Self::Withdrawn,
            _ => Self::Unknown,
        }
    }
}

impl ContributionStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Waiting for review",
            Self::Accepted => "Counted",
            Self::Quarantined => "Set aside",
            Self::Withdrawn => "Withdrawn",
            Self::Unknown => "Awaiting an update",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ImageFeedView {
    id: String,
    status: ImageStatus,
    reason: Option<String>,
    rms_de: Option<f64>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(from = "String")]
enum ImageStatus {
    Pending,
    Accepted,
    DownWeighted,
    Quarantined,
    Unknown,
}

impl From<String> for ImageStatus {
    fn from(value: String) -> Self {
        match value.as_str() {
            "pending" => Self::Pending,
            "accepted" => Self::Accepted,
            "down_weighted" => Self::DownWeighted,
            "quarantined" => Self::Quarantined,
            _ => Self::Unknown,
        }
    }
}

impl ImageStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "Waiting for review",
            Self::Accepted => "Counted",
            Self::DownWeighted => "Counted less",
            Self::Quarantined => "Set aside",
            Self::Unknown => "Awaiting an update",
        }
    }
}

#[component]
pub fn StatusPage(#[prop(optional)] token_override: Option<String>) -> impl IntoView {
    let params = use_params_map();
    let session = crate::session::use_session();
    let navigate = leptos_router::hooks::use_navigate();
    let from_link = token_override.is_none();
    let token = Signal::derive(move || {
        token_override
            .clone()
            .unwrap_or_else(|| params.get().get("token").unwrap_or_default())
    });
    let state = RwSignal::new(Load::<StatusResponse>::Loading);

    let available = crate::base::server_available();

    Effect::new(move |_| {
        if !available {
            return;
        }
        let requested = token.get();
        let navigate = navigate.clone();
        spawn_local(async move {
            if from_link {
                let result = async {
                    let status_token = crate::webauthn::StatusToken::try_from(requested.clone())?;
                    crate::webauthn::restore_session(&status_token)
                        .await
                        .map_err(|error| error.to_string())
                }
                .await;
                if token.get_untracked() != requested {
                    return;
                }
                match result {
                    Ok(profile) => {
                        match session.set_status_link(profile).await {
                            Ok(()) => navigate("/me", Default::default()),
                            Err(_) => state.set(Load::Failed("This browser couldn't save the sign-in link. Allow site storage and try again.".to_owned())),
                        }
                    }
                    Err(_) => state.set(Load::Failed("This status link no longer works. Your existing sign-in has not changed. Sign in with your passkey to see your contributions.".to_owned())),
                }
                return;
            }
            let result = async {
                let response = Request::get(&crate::base::api_url(&format!("/status/{requested}")))
                    .send().await.map_err(|error| error.to_string())?;
                if !response.ok() {
                    if matches!(response.status(), 401 | 403 | 404)
                        && matches!(session.state.get_untracked(), crate::session::SessionState::SignedIn(profile) if profile.status_token.as_str() == requested)
                    {
                        session.sign_out().await?;
                    }
                    return Err("Couldn't load your contributions. Try signing in again.".to_owned());
                }
                response.json::<StatusResponse>().await.map_err(|error| error.to_string())
            }.await;
            if token.get_untracked() == requested {
                state.set(match result {
                    Ok(response) => Load::Ready(response),
                    Err(error) => Load::Failed(error),
                });
            }
        });
    });

    view! {
        <h1>"Your contributions"</h1>
        {(!available).then(ui::preview_notice)}
        <p class="lede" hidden=move || !available>
            "Keep your status link private. Anyone with the link can read your contributions. "
            <a href=crate::base::site_url("/help/passkeys")>"How sign-in works"</a> "."
        </p>
        {move || available.then(|| {
            let token = token.get();
            let feed = crate::base::api_url(&format!("/status/{token}.rss"));
            let href = feed.clone();
            view! {
                <p class="status-links">
                    <a href=crate::base::site_url(&format!("/status/{token}"))>"Private status link to bookmark"</a>
                    <a href=href>"Subscribe to updates with RSS"</a>
                </p>
            }
        })}
        {move || {
            if !available {
                return ().into_any();
            }
            match state.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(response) => view! {
                    <Feed response/>
                }.into_any(),
            }
        }}
    }
}

#[component]
fn Feed(response: StatusResponse) -> impl IntoView {
    view! {
        {if response.contributions.is_empty() {
            view! {
                <section class="panel">
                    <h2>"Your first contribution starts here"</h2>
                    <p>"Share RAW and JPEG pairs to help measure your camera's looks. Your photos stay on your device."</p>
                    <a href=crate::base::site_url("/contribute")>"See ways to help"</a>
                </section>
            }.into_any()
        } else {
            response
                .contributions
                .into_iter()
                .map(|contribution| view! {
                    <section class="panel">
                        <Withdrawal contribution_id=contribution.id.clone()/>
                        <h2>{format!("Contribution {}", contribution.id)}</h2>
                        <p>
                            {contribution.status.as_str()}
                            " · "
                            {format_date(contribution.created_at)}
                        </p>
                        <table class="data-table">
                            <thead>
                                <tr>
                                    <th scope="col">Image</th>
                                    <th scope="col">Status</th>
                                    <th scope="col">What happens next</th>
                                    <th scope="col">Details</th>
                                </tr>
                            </thead>
                            <tbody>
                                {contribution.images.into_iter().map(|image| view! {
                                    <tr>
                                        <th scope="row">{image.id}</th>
                                        <td>{image.status.as_str()}</td>
                                        <td>{image.reason.unwrap_or_default()}</td>
                                        <td>
                                            <ui::Disclosure summary="Measurements".to_owned()>
                                                <p>{image.rms_de.map(|value| format!("RMS ΔE: {value:.2}")).unwrap_or_else(|| "Measurements are not available yet.".to_owned())}</p>
                                            </ui::Disclosure>
                                        </td>
                                    </tr>
                                }).collect_view()}
                            </tbody>
                        </table>
                    </section>
                })
                .collect_view()
                .into_any()
        }}
    }
}

#[component]
fn Withdrawal(contribution_id: ContributionId) -> impl IntoView {
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let done = RwSignal::new(false);
    view! {
        <button disabled=move || busy.get() || done.get() on:click=move |_| {
            let id = contribution_id.clone();
            busy.set(true);
            spawn_local(async move {
                let result = async {
                    let signed = crate::webauthn::sign_request(crate::request_digest::RequestDomain::Withdraw, id.0.as_bytes(), None).await?;
                    #[derive(serde::Serialize)]
                    struct RequestBody<'a> { #[serde(flatten)] signed: &'a crate::webauthn::SignedFields, contribution_id: &'a ContributionId }
                    let response = Request::post(&crate::base::api_url("/api/withdraw")).json(&RequestBody { signed: &signed.fields, contribution_id: &id }).map_err(|error| error.to_string())?.send().await.map_err(|error| error.to_string())?;
                    if !response.ok() { return Err("Withdrawal did not finish. Try again with the profile that shared these photos.".to_owned()); }
                    Ok(())
                }.await;
                match result {
                    Ok(()) => { done.set(true); message.set(Some("Withdrawn. These samples will no longer count toward new models.".to_owned())); }
                    Err(_) => message.set(Some("Withdrawal did not finish. Try again with the profile that shared these photos.".to_owned())),
                }
                busy.set(false);
            });
        }>"Withdraw contribution"</button>
        {move || message.get().map(|message| view! {
            <p role="status">{message}</p>
        })}
    }
}

fn format_date(seconds: i64) -> String {
    let millis = (seconds as f64) * 1000.0;
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(millis))
        .to_iso_string()
        .as_string()
        .unwrap_or_default()
}
