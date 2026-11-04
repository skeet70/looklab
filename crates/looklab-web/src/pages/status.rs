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
    credential: String,
    contributions: Vec<ContributionView>,
}

#[derive(Clone, Debug, Deserialize)]
struct ContributionView {
    id: String,
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
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Quarantined => "quarantined",
            Self::Withdrawn => "withdrawn",
            Self::Unknown => "unknown",
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
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::DownWeighted => "down-weighted",
            Self::Quarantined => "quarantined",
            Self::Unknown => "unknown",
        }
    }
}

#[component]
pub fn StatusPage() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<StatusResponse>::Loading);

    Effect::new(move |_| {
        let token = params.get().get("token").unwrap_or_default();
        spawn_local(async move {
            let result = async {
                let response = Request::get(&format!("/status/{token}"))
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
                if !response.ok() {
                    return Err(format!("status: HTTP {}", response.status()));
                }
                response
                    .json::<StatusResponse>()
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            state.set(match result {
                Ok(response) => Load::Ready(response),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <h1>"My contributions"</h1>
        <p class="lede">
            "This link is a capability token: anyone who has it can read this page. It is not your passkey. "
            "Lost it? "
            <a href="/passkeys">"Recover it with your passkey"</a> "."
        </p>
        {move || {
            let token = params.get().get("token").unwrap_or_default();
            view! {
                <p class="muted">
                    "RSS: "
                    <a href=format!("/status/{token}.rss")>{format!("/status/{token}.rss")}</a>
                </p>
            }
        }}
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(response) => view! { <Feed response/> }.into_any(),
        }}
    }
}

#[component]
fn Feed(response: StatusResponse) -> impl IntoView {
    view! {
        <p class="muted">{format!("Credential {}", response.credential)}</p>
        {if response.contributions.is_empty() {
            view! { <p>"No contributions yet."</p> }.into_any()
        } else {
            response
                .contributions
                .into_iter()
                .map(|contribution| view! {
                    <section class="panel">
                        <h2>{format!("Contribution {}", contribution.id)}</h2>
                        <p>
                            {contribution.status.as_str()}
                            " · "
                            {format_date(contribution.created_at)}
                        </p>
                        <table class="data-table">
                            <thead>
                                <tr><th scope="col">Image</th><th scope="col">Status</th><th scope="col">Reason</th><th scope="col">RMS ΔE</th></tr>
                            </thead>
                            <tbody>
                                {contribution.images.into_iter().map(|image| view! {
                                    <tr>
                                        <th scope="row">{image.id}</th>
                                        <td>{image.status.as_str()}</td>
                                        <td>{image.reason.unwrap_or_default()}</td>
                                        <td>{image.rms_de.map(|value| format!("{value:.2}")).unwrap_or_default()}</td>
                                    </tr>
                                }).collect_view()}
                            </tbody>
                        </table>
                    </section>
                })
                .collect_view()
                .into_any()
        }}
        <p class="muted">
            "Withdrawal is a request signed by the contributing credential. Use the CLI for now: "
            <code>"looklab withdraw"</code> "."
        </p>
    }
}

fn format_date(seconds: i64) -> String {
    let millis = (seconds as f64) * 1000.0;
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(millis))
        .to_iso_string()
        .as_string()
        .unwrap_or_default()
}
