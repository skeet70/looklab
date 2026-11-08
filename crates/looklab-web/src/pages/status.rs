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
    activities: Vec<ActivityView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ActivitySource {
    Send,
    Activity,
}

#[derive(Clone, Debug, Deserialize)]
struct ActivityView {
    source: ActivitySource,
    created_at: i64,
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

impl PartialEq for ContributionId {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl Eq for ContributionId {}

impl PartialOrd for ContributionId {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ContributionId {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct ContributionView {
    id: ContributionId,
    status: ContributionStatus,
    render_class: RenderClass,
    deferred_until: Option<i64>,
    created_at: i64,
    body_id: String,
    intent: IntentView,
    target_look: Option<String>,
    sim_key: String,
    images: Vec<ImageFeedView>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum IntentView {
    Native,
    Recipe,
    Card,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(from = "String")]
enum RenderClass {
    Colour,
    Monochrome,
    Unknown,
}

impl From<String> for RenderClass {
    fn from(value: String) -> Self {
        match value.as_str() {
            "colour" => Self::Colour,
            "monochrome" => Self::Monochrome,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
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
        {if response.activities.is_empty() {
            view! {
                <section class="panel">
                    <h2>"Your first contribution starts here"</h2>
                    <p>"Share RAW and JPEG pairs to help measure your camera's looks. Your photos stay on your device."</p>
                    <a href=crate::base::site_url("/contribute")>"See ways to help"</a>
                </section>
            }.into_any()
        } else {
            response
                .activities
                .into_iter()
                .map(activity_panel)
                .collect_view()
                .into_any()
        }}
    }
}

fn activity_panel(activity: ActivityView) -> impl IntoView {
    let summary = ActivitySummary::of(&activity);
    let kind = match activity.source {
        ActivitySource::Send => "Submission",
        ActivitySource::Activity => "Earlier activity",
    };
    let heading = format_day(activity.created_at);
    let meta = summary.meta_line();
    let (label, detail) = summary.status_line();
    let reasons = summary.reasons.clone();
    let deferred_until = summary.deferred_until;
    let withdrawn =
        summary.contributions > 0 && summary.withdrawn_contributions == summary.contributions;
    let contribution_ids = activity
        .contributions
        .iter()
        .map(|contribution| contribution.id.clone())
        .collect::<Vec<_>>();
    let technical = activity.contributions;
    view! {
        <section class="panel contribution-activity">
            <h2>{heading}</h2>
            <p class="activity-meta muted">{format!("{kind} · {meta}")}</p>
            <p class="activity-status"><strong>{label}</strong> " · " {detail}</p>
            {(!reasons.is_empty()).then(move || view! {
                <ul class="activity-reasons">
                    {reasons.into_iter().map(|reason| view! { <li>{reason}</li> }).collect_view()}
                </ul>
            })}
            {deferred_until.map(|until| view! {
                <p class="muted">{format!(
                    "Kept until {} for monochrome model development, then deleted unless the measurements enter monochrome analysis first. No published Look is promised.",
                    format_day(until)
                )}</p>
            })}
            {(!withdrawn).then(move || view! {
                <div class="field-row"><Withdrawal contribution_ids=contribution_ids/></div>
            })}
            <ui::Disclosure summary="Technical details".to_owned()>
                {technical_table(technical)}
            </ui::Disclosure>
        </section>
    }
}

#[derive(Default)]
struct ActivitySummary {
    contributions: usize,
    withdrawn_contributions: usize,
    photos: usize,
    cameras: Vec<String>,
    looks: Vec<String>,
    accepted: usize,
    down_weighted: usize,
    quarantined: usize,
    pending: usize,
    unknown: usize,
    withdrawn: usize,
    deferred: usize,
    deferred_until: Option<i64>,
    reasons: Vec<String>,
}

impl ActivitySummary {
    fn of(activity: &ActivityView) -> Self {
        let mut summary = Self::default();
        for contribution in &activity.contributions {
            summary.contributions += 1;
            summary.photos += contribution.images.len();
            push_unique(&mut summary.cameras, camera_label(&contribution.body_id));
            push_unique(&mut summary.looks, look_label(contribution));
            if contribution.status == ContributionStatus::Withdrawn {
                summary.withdrawn_contributions += 1;
                summary.withdrawn += contribution.images.len();
                continue;
            }
            if contribution.deferred() {
                summary.deferred += contribution.images.len();
                summary.deferred_until = summary.deferred_until.max(contribution.deferred_until);
                continue;
            }
            for image in &contribution.images {
                match image.status {
                    ImageStatus::Accepted => summary.accepted += 1,
                    ImageStatus::DownWeighted => summary.down_weighted += 1,
                    ImageStatus::Quarantined => summary.quarantined += 1,
                    ImageStatus::Pending => {
                        if contribution.status == ContributionStatus::Quarantined {
                            summary.quarantined += 1;
                        } else {
                            summary.pending += 1;
                        }
                    }
                    ImageStatus::Unknown => {
                        if contribution.status == ContributionStatus::Quarantined {
                            summary.quarantined += 1;
                        } else {
                            summary.unknown += 1;
                        }
                    }
                }
                if let Some(reason) = &image.reason {
                    push_unique(&mut summary.reasons, reason.clone());
                }
            }
        }
        summary
    }

    fn meta_line(&self) -> String {
        let mut parts = vec![format!(
            "{} {}",
            self.photos,
            if self.photos == 1 { "photo" } else { "photos" }
        )];
        if !self.cameras.is_empty() {
            parts.push(if self.cameras.len() == 1 {
                format!("Camera {}", self.cameras[0])
            } else {
                format!("Cameras {}", self.cameras.join(", "))
            });
        }
        if !self.looks.is_empty() {
            parts.push(self.looks.join(", "));
        }
        parts.join(" · ")
    }

    fn status_line(&self) -> (String, String) {
        if self.contributions > 0 && self.withdrawn_contributions == self.contributions {
            return (
                "Withdrawn".to_owned(),
                "These photos no longer count toward new models.".to_owned(),
            );
        }
        let mut counts = Vec::new();
        if self.accepted > 0 {
            counts.push(format!("{} counted", self.accepted));
        }
        if self.down_weighted > 0 {
            counts.push(format!("{} counted less", self.down_weighted));
        }
        if self.quarantined > 0 {
            counts.push(format!("{} set aside", self.quarantined));
        }
        if self.pending > 0 {
            counts.push(format!("{} awaiting a fit", self.pending));
        }
        if self.unknown > 0 {
            counts.push(format!("{} awaiting an update", self.unknown));
        }
        if self.deferred > 0 {
            counts.push(format!("{} awaiting monochrome analysis", self.deferred));
        }
        if self.withdrawn > 0 {
            counts.push(format!("{} withdrawn", self.withdrawn));
        }
        let label = if self.accepted > 0 || self.down_weighted > 0 {
            "Checked"
        } else if self.pending > 0 || self.deferred > 0 {
            "Received"
        } else if self.quarantined > 0 {
            "Set aside"
        } else if self.withdrawn > 0 {
            "Partly withdrawn"
        } else {
            "Awaiting an update"
        };
        let detail = if counts.is_empty() {
            "Waiting for status.".to_owned()
        } else {
            counts.join(" · ")
        };
        (label.to_owned(), detail)
    }
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

fn camera_label(body_id: &str) -> String {
    let (make, model) = body_id.split_once('/').unwrap_or(("", body_id));
    let make = title_words(make);
    let model = model
        .split('-')
        .map(capitalize)
        .collect::<Vec<_>>()
        .join("-");
    match (make.is_empty(), model.is_empty()) {
        (true, _) => model,
        (_, true) => make,
        _ => format!("{make} {model}"),
    }
}

fn look_label(contribution: &ContributionView) -> String {
    match contribution.intent {
        IntentView::Native => format!("Built-in {}", title_words(&contribution.sim_key)),
        IntentView::Recipe => contribution
            .target_look
            .as_deref()
            .map(|look| format!("Recipe for {}", title_words(look)))
            .unwrap_or_else(|| "Recipe".to_owned()),
        IntentView::Card => "Card scan".to_owned(),
    }
}

fn title_words(value: &str) -> String {
    value
        .split(['-', '_'])
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ")
}

fn capitalize(value: &str) -> String {
    let mut characters = value.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => String::new(),
    }
}

impl ContributionView {
    fn deferred(&self) -> bool {
        self.render_class == RenderClass::Monochrome && self.status == ContributionStatus::Pending
    }
}

fn technical_table(contributions: Vec<ContributionView>) -> impl IntoView {
    view! {
        <table class="data-table">
            <caption>"Contribution records"</caption>
            <thead>
                <tr>
                    <th scope="col">"Contribution"</th>
                    <th scope="col">"Image"</th>
                    <th scope="col">"Received"</th>
                    <th scope="col">"Contribution status"</th>
                    <th scope="col">"Render class"</th>
                    <th scope="col">"Kept until"</th>
                    <th scope="col">"Image status"</th>
                    <th scope="col">"RMS ΔE"</th>
                    <th scope="col">"Reason"</th>
                </tr>
            </thead>
            <tbody>
                {contributions
                    .into_iter()
                    .flat_map(|contribution| {
                        let id = contribution.id.to_string();
                        let received = format_date(contribution.created_at);
                        let deferred = contribution.deferred();
                        let status = if deferred {
                            "received, awaiting monochrome analysis".to_owned()
                        } else {
                            contribution.status.as_str().to_owned()
                        };
                        let render = match contribution.render_class {
                            RenderClass::Colour => "Colour",
                            RenderClass::Monochrome => "B&W",
                            RenderClass::Unknown => "Unknown",
                        };
                        let kept = if deferred {
                            contribution
                                .deferred_until
                                .map(format_date)
                                .unwrap_or_else(|| "—".to_owned())
                        } else {
                            "—".to_owned()
                        };
                        if contribution.images.is_empty() {
                            vec![view! {
                                <tr>
                                    <th scope="row">{id.clone()}</th>
                                    <td>"—"</td>
                                    <td>{received.clone()}</td>
                                    <td>{status.clone()}</td>
                                    <td>{render}</td>
                                    <td>{kept.clone()}</td>
                                    <td>"—"</td>
                                    <td>"—"</td>
                                    <td></td>
                                </tr>
                            }.into_any()]
                        } else {
                            contribution
                                .images
                                .into_iter()
                                .map(|image| {
                                    let image_status = if deferred {
                                        "received, awaiting monochrome analysis".to_owned()
                                    } else {
                                        image.status.as_str().to_owned()
                                    };
                                    let rms = image
                                        .rms_de
                                        .map(|value| format!("{value:.2}"))
                                        .unwrap_or_else(|| "—".to_owned());
                                    let reason = image.reason.unwrap_or_default();
                                    view! {
                                        <tr>
                                            <th scope="row">{id.clone()}</th>
                                            <td>{image.id}</td>
                                            <td>{received.clone()}</td>
                                            <td>{status.clone()}</td>
                                            <td>{render}</td>
                                            <td>{kept.clone()}</td>
                                            <td>{image_status}</td>
                                            <td>{rms}</td>
                                            <td>{reason}</td>
                                        </tr>
                                    }
                                    .into_any()
                                })
                                .collect::<Vec<_>>()
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

#[component]
fn Withdrawal(contribution_ids: Vec<ContributionId>) -> impl IntoView {
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let done = RwSignal::new(false);
    view! {
        <button type="button" disabled=move || busy.get() || done.get() on:click=move |_| {
            let mut ids = contribution_ids.clone();
            busy.set(true);
            spawn_local(async move {
                ids.sort();
                ids.dedup();
                let capacity = ids.iter().map(|id| id.0.len()).sum::<usize>()
                    + ids.len().saturating_sub(1);
                let mut canonical = String::with_capacity(capacity);
                for id in &ids {
                    if !canonical.is_empty() {
                        canonical.push('\n');
                    }
                    canonical.push_str(&id.0);
                }
                let result = async {
                    let signed = crate::webauthn::sign_request(crate::request_digest::RequestDomain::Withdraw, canonical.as_bytes(), None).await?;
                    #[derive(serde::Serialize)]
                    struct RequestBody<'a> { #[serde(flatten)] signed: &'a crate::webauthn::SignedFields, contribution_ids: &'a [ContributionId] }
                    let response = Request::post(&crate::base::api_url("/api/withdraw")).json(&RequestBody { signed: &signed.fields, contribution_ids: ids.as_slice() }).map_err(|error| error.to_string())?.send().await.map_err(|error| error.to_string())?;
                    if !response.ok() { return Err("Withdrawal did not finish. Try again with the profile that shared these photos.".to_owned()); }
                    Ok(())
                }.await;
                match result {
                    Ok(()) => { done.set(true); message.set(Some("Withdrawn. These photos no longer count toward new models.".to_owned())); }
                    Err(_) => message.set(Some("Withdrawal did not finish. Try again with the profile that shared these photos.".to_owned())),
                }
                busy.set(false);
            });
        }>"Withdraw these photos"</button>
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

fn format_day(seconds: i64) -> String {
    let millis = (seconds as f64) * 1000.0;
    let text = js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(millis))
        .to_locale_date_string("en", &wasm_bindgen::JsValue::UNDEFINED)
        .as_string()
        .unwrap_or_default();
    if text.is_empty() {
        format_date(seconds)
    } else {
        text
    }
}
