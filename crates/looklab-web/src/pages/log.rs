// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_publish::{AuditEvent, Decision, LookListing, Role, SplitKind};

use crate::data::{self, Load};
use crate::ui;

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Seeder => "trusted contributor",
        Role::Moderator => "moderator",
        Role::Admin => "administrator",
    }
}

pub(super) fn event_sentence(event: &AuditEvent) -> String {
    match event {
        AuditEvent::Grant {
            subject,
            role,
            granted_by,
            reason,
        } => format!(
            "{granted_by} made {subject} a {}: {reason}",
            role_name(*role)
        ),
        AuditEvent::Revocation {
            subject,
            role,
            revoked_by,
            reason,
        } => format!(
            "{revoked_by} removed {subject}'s {} role: {reason}",
            role_name(*role)
        ),
        AuditEvent::Endorsement {
            issuer,
            subject,
            reason,
        } => format!("{issuer} vouched for {subject}: {reason}"),
        AuditEvent::RootAdmission {
            admitted_by,
            contribution,
            reason,
        } => format!(
            "{admitted_by} accepted contribution {contribution} as reference data: {reason}"
        ),
        AuditEvent::ModerationDecision {
            proposal,
            moderator,
            decision,
            reason,
        } => format!(
            "{moderator} {} proposal {proposal}: {reason}",
            match decision {
                Decision::Approve => "approved",
                Decision::Reject => "rejected",
                Decision::RequestInfo => "requested information on",
            }
        ),
        AuditEvent::ReAnchor {
            look,
            version,
            reason,
        } => format!("{look} version {version} received new reference data: {reason}"),
        AuditEvent::Revert {
            look,
            to_version,
            reason,
            ..
        } => format!("{look} returned to version {to_version}: {reason}"),
        AuditEvent::Split { kind, detail } => format!(
            "{} separated: {detail}",
            match kind {
                SplitKind::Epoch => "Firmware versions",
                SplitKind::Look => "Looks",
            }
        ),
        AuditEvent::LookListing {
            look,
            state,
            moderator,
            reason,
        } => format!(
            "{moderator} marked {look} {}: {reason}",
            match state {
                LookListing::Listed => "listed",
                LookListing::Hidden => "hidden",
                LookListing::Removed => "removed",
            }
        ),
        AuditEvent::Release { look, version, .. } => format!("{look} {version} published."),
    }
}

#[component]
pub fn CommunityPage() -> impl IntoView {
    view! {
        <div class="community-page">
            <header class="community-hero">
                <p class="eyebrow">"Community"</p>
                <h1>"Better looks, built together"</h1>
                <p class="lede">"Share what your camera can do. Help everyone find a closer match."</p>
                <a class="button" href=crate::base::site_url("/community/propose")>
                    "Propose a change"
                </a>
            </header>
            <section class="community-principles" aria-label="How decisions are made">
                <article>
                    <h2>"Anyone can help"</h2>
                    <p>"Share photos, add a camera or tell us about a film simulation we missed."</p>
                    <a href=crate::base::site_url("/contribute")>"Ways to help"</a>
                </article>
                <article>
                    <h2>"Trust grows with evidence"</h2>
                    <p>"New contributors' data has a limited influence until a trusted member vouches for them."</p>
                </article>
                <article>
                    <h2>"Decisions stay public"</h2>
                    <p>"Moderators check changes to camera data against the evidence. Confirmations help them decide; they are not votes."</p>
                </article>
            </section>
            <h2>"Recent activity"</h2>
            <ActivityList recent=true/>
            <nav class="community-links" aria-label="Community resources">
                <a href=crate::base::site_url("/community/log")>"Full audit log and JSON"</a>
                <a href=crate::base::site_url("/review")>"Moderator sign-in"</a>
            </nav>
        </div>
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActivityFilter {
    All,
    Decisions,
    Releases,
    Grants,
    Endorsements,
    Revocations,
}

impl ActivityFilter {
    fn parse(value: &str) -> Self {
        match value {
            "moderation_decision" => Self::Decisions,
            "release" => Self::Releases,
            "grant" => Self::Grants,
            "endorsement" => Self::Endorsements,
            "revocation" => Self::Revocations,
            _ => Self::All,
        }
    }

    fn query_value(self) -> &'static str {
        match self {
            Self::All => "",
            Self::Decisions => "moderation_decision",
            Self::Releases => "release",
            Self::Grants => "grant",
            Self::Endorsements => "endorsement",
            Self::Revocations => "revocation",
        }
    }

    fn includes(self, category: Self) -> bool {
        self == Self::All || self == category
    }
}

#[component]
fn ActivityList(#[prop(default = false)] recent: bool) -> impl IntoView {
    let state = RwSignal::new(Load::<Vec<ActivityItem>>::Loading);
    let query = leptos_router::hooks::use_query_map();
    let navigate = leptos_router::hooks::use_navigate();
    let filter =
        Signal::derive(move || ActivityFilter::parse(&query.get().get("show").unwrap_or_default()));
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match load_activity().await {
                Ok(log) => Load::Ready(log),
                Err(error) => Load::Failed(error),
            });
        });
    });
    view! {
        {(!recent).then(move || view! {
            <label for="activity-filter">"Show activity"</label>
            <select
                id="activity-filter"
                prop:value=move || filter.get().query_value()
                on:change=move |event| {
                    let selected = ActivityFilter::parse(&event_target_value(&event));
                    if let Ok(url) = web_sys::Url::new(&window().location().href().unwrap_or_default()) {
                        if selected == ActivityFilter::All {
                            url.search_params().delete("show");
                        } else {
                            url.search_params().set("show", selected.query_value());
                        }
                        navigate(
                            &crate::base::route_of(&url),
                            leptos_router::NavigateOptions {
                                scroll: false,
                                ..Default::default()
                            },
                        );
                    }
                }
            >
                <option value="">"All activity"</option>
                <option value="moderation_decision">"Proposal decisions"</option>
                <option value="release">"Published looks"</option>
                <option value="grant">"Role grants"</option>
                <option value="endorsement">"Endorsements"</option>
                <option value="revocation">"Role removals"</option>
            </select>
        })}
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(log) => {
                let selected = filter.get();
                let entries = log.iter().rev().filter(|entry| selected.includes(entry.category)).take(if recent { 6 } else { usize::MAX }).map(|entry| {
                    let sentence = entry.sentence.clone();
                    let headline = match &entry.proposal {
                        Some(proposal) => view! {
                            <a href=crate::base::site_url(&format!("/proposals/{}", proposal.as_str()))>
                                {sentence}
                            </a>
                        }.into_any(),
                        None => sentence.into_any(),
                    };
                    view! {
                        <li>
                            <p>{headline}</p>
                            {(!entry.reason.is_empty()).then(|| view! {
                                <p class="muted">{entry.reason.clone()}</p>
                            })}
                            <small class="muted">{local_activity_date(&entry.time)}</small>
                            {(!recent).then(|| view! {
                                <div class="activity-detail">
                                    <small class="muted">{format!("{} · #{}", entry.time, entry.seq)}</small>
                                    <code>{entry.detail.clone()}</code>
                                </div>
                            })}
                        </li>
                    }
                }).collect_view();
                view! {
                    <ol class="activity-list">{entries}</ol>
                    {log.is_empty().then(|| view! {
                        <p class="muted">
                            "No decisions yet. Propose a change to help build the camera library."
                        </p>
                    })}
                }.into_any()
            }
        }}
    }
}

#[component]
pub fn LogPage() -> impl IntoView {
    let json_url = if crate::base::server_available() {
        crate::base::api_url("/log.json")
    } else {
        crate::base::site_url("/data/audit-log.json")
    };
    view! {
        <div class="community-page">
            <p class="eyebrow">"Community"</p>
            <h1>"Public audit log"</h1>
            <p class="lede">
                "Every decision has a reason. This history includes published looks, proposal decisions, role grants and endorsements."
            </p>
            <p>
                <a href=json_url>"Download JSON"</a>
                " · "
                <a href=crate::base::site_url("/community")>"How decisions are made"</a>
            </p>
            <ActivityList/>
            <PublicList endorsements=false/>
            <PublicList endorsements=true/>
        </div>
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
struct ListRecord {
    subject: looklab_publish::CredentialRef,
    subject_handle: Option<String>,
    reason: String,
    revoked_at: Option<i64>,
    #[serde(default)]
    grant: Option<looklab_trust::Grant>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct RecordPage {
    #[serde(default)]
    grants: Vec<ListRecord>,
    #[serde(default)]
    endorsements: Vec<ListRecord>,
    next_after: Option<PublicRecordId>,
}

#[derive(Clone, Debug, serde::Deserialize)]
struct PublicRecordId(String);

#[component]
pub(super) fn PublicList(endorsements: bool) -> impl IntoView {
    let state = RwSignal::new(Load::<RecordPage>::Loading);
    let path = if endorsements {
        "/lists/endorsements.json"
    } else {
        "/lists/grants.json"
    };
    let load = move |after: Option<PublicRecordId>| {
        leptos::task::spawn_local(async move {
            let request = gloo_net::http::Request::get(&crate::base::api_url(path));
            let request = match after {
                Some(after) => request.query([("after", after.0)]),
                None => request,
            };
            let result = async {
                let response = request.send().await.map_err(|error| error.to_string())?;
                if !response.ok() {
                    return Err("This public list could not be loaded. Try again later.".to_owned());
                }
                response
                    .json::<RecordPage>()
                    .await
                    .map_err(|error| error.to_string())
            }
            .await;
            state.set(match result {
                Ok(page) => Load::Ready(page),
                Err(error) => Load::Failed(error),
            });
        });
    };
    Effect::new(move |_| {
        if crate::base::server_available() {
            load(None);
        }
    });
    view! {
        <section id=if endorsements { "endorsements" } else { "grants" }>
            <h2>{if endorsements { "Public endorsements" } else { "Public role grants" }}</h2>
            <p>{if endorsements { "Trusted contributors who have vouched for another contributor." } else { "The people entrusted with community roles." }}</p>
            <a href=crate::base::api_url(path)>"Download this list as JSON"</a>
            {move || {
                if !crate::base::server_available() {
                    return view! {
                        <p class="muted">"Public lists are available on the live site."</p>
                    }.into_any();
                }
                match state.get() {
                    Load::Loading => ui::loading().into_any(),
                    Load::Failed(error) => ui::failure(&error).into_any(),
                    Load::Ready(page) => {
                        let records = if endorsements { page.endorsements } else { page.grants };
                        let empty = records.is_empty();
                        view! {
                            <ul class="activity-list">
                                {records.into_iter().map(|record| {
                                    let (role, fallback) = match record.grant {
                                        Some(looklab_trust::Grant::Admin) => ("administrator", "An administrator"),
                                        Some(looklab_trust::Grant::Moderator) => ("moderator", "A moderator"),
                                        Some(looklab_trust::Grant::Seeder) => ("seeder", "A seeder"),
                                        None => ("endorsed contributor", "A trusted contributor"),
                                    };
                                    let name = record.subject_handle.unwrap_or_else(|| fallback.to_owned());
                                    view! {
                                        <li>
                                            <p>
                                                <strong>{name}</strong>
                                                {format!(" · {role}{}", if record.revoked_at.is_some() { " (no longer active)" } else { "" })}
                                            </p>
                                            <p>{record.reason}</p>
                                            <small class="muted">
                                                <code>{record.subject.to_string()}</code>
                                            </small>
                                        </li>
                                    }
                                }).collect_view()}
                            </ul>
                            {empty.then(|| view! {
                                <p class="muted">"No entries yet."</p>
                            })}
                            {page.next_after.map(|after| view! {
                                <button type="button" on:click=move |_| load(Some(after.clone()))>
                                    "Next entries"
                                </button>
                            })}
                        }.into_any()
                    }
                }
            }}
        </section>
    }
}

#[derive(Clone, Debug)]
struct ActivityItem {
    sentence: String,
    reason: String,
    detail: String,
    proposal: Option<looklab_publish::ProposalRef>,
    time: String,
    seq: i64,
    category: ActivityFilter,
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LiveEventKind {
    GrantCreated,
    GrantRevoked,
    EndorsementIssued,
    EndorsementRevoked,
    EndorsementSuperseded,
    RootAdmitted,
    RootRevoked,
    ModerationDecision,
    ProposalOpened,
    ReAnchor,
    Revert,
    Split,
    ReleaseManifest,
    #[serde(other)]
    Unknown,
}

#[derive(serde::Deserialize)]
struct LivePayload {
    event: LiveEventKind,
    actor: Option<looklab_publish::CredentialRef>,
    subject: Option<looklab_publish::CredentialRef>,
    proposal: Option<looklab_publish::ProposalRef>,
    look: Option<looklab_publish::LookRef>,
    #[serde(default)]
    reason: String,
    decision: Option<LiveDecision>,
    #[serde(alias = "moderator_handle")]
    actor_handle: Option<crate::webauthn::DisplayHandle>,
    #[serde(alias = "human_summary")]
    summary: Option<AuditSummary>,
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum AuditSummary {
    Menu(looklab_schema::ProposalSummary),
    Text(String),
}

impl AuditSummary {
    fn headline(&self) -> &str {
        match self {
            Self::Menu(summary) => &summary.headline,
            Self::Text(summary) => summary.lines().next().unwrap_or_default(),
        }
    }
}

fn local_activity_date(iso: &str) -> String {
    let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(iso));
    if !date.get_time().is_finite() {
        return "Date unavailable".to_owned();
    }
    let month = match date.get_month() {
        0 => "Jan",
        1 => "Feb",
        2 => "Mar",
        3 => "Apr",
        4 => "May",
        5 => "Jun",
        6 => "Jul",
        7 => "Aug",
        8 => "Sep",
        9 => "Oct",
        10 => "Nov",
        _ => "Dec",
    };
    format!(
        "{} {month}, {:02}:{:02}",
        date.get_date(),
        date.get_hours(),
        date.get_minutes()
    )
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LiveDecision {
    Approve,
    Reject,
    RequestInfo,
}

impl LiveDecision {
    fn wording(self) -> &'static str {
        match self {
            Self::Approve => "approved",
            Self::Reject => "rejected",
            Self::RequestInfo => "requested information on",
        }
    }
}

#[derive(serde::Deserialize)]
struct LiveEntry {
    seq: i64,
    ts: i64,
    payload: LivePayload,
}

#[derive(serde::Deserialize)]
struct LivePage {
    entries: Vec<LiveEntry>,
    next_after: Option<i64>,
}

async fn load_activity() -> Result<Vec<ActivityItem>, String> {
    if !crate::base::server_available() {
        return Ok(data::audit_log()
            .await?
            .entries
            .iter()
            .map(|entry| {
                let (sentence, reason, proposal, category) = match &entry.event {
                    AuditEvent::Grant { role, reason, .. } => (
                        format!("A contributor received a {} role.", role_name(*role)),
                        reason.clone(),
                        None,
                        ActivityFilter::Grants,
                    ),
                    AuditEvent::Revocation { role, reason, .. } => (
                        format!("A contributor's {} role was removed.", role_name(*role)),
                        reason.clone(),
                        None,
                        ActivityFilter::Revocations,
                    ),
                    AuditEvent::Endorsement { reason, .. } => (
                        "A trusted contributor vouched for another contributor.".to_owned(),
                        reason.clone(),
                        None,
                        ActivityFilter::Endorsements,
                    ),
                    AuditEvent::ModerationDecision {
                        proposal,
                        decision,
                        reason,
                        ..
                    } => (
                        format!(
                            "A moderator {} a proposal.",
                            match decision {
                                Decision::Approve => "approved",
                                Decision::Reject => "rejected",
                                Decision::RequestInfo => "requested information on",
                            }
                        ),
                        reason.clone(),
                        Some(proposal.clone()),
                        ActivityFilter::Decisions,
                    ),
                    AuditEvent::LookListing { look, reason, .. } => (
                        format!("{look} changed its listing state."),
                        reason.clone(),
                        None,
                        ActivityFilter::All,
                    ),
                    AuditEvent::Release { look, version, .. } => (
                        format!("{look} {version} published."),
                        String::new(),
                        None,
                        ActivityFilter::Releases,
                    ),
                    AuditEvent::RootAdmission { reason, .. } => (
                        "A trusted contributor accepted new reference data.".to_owned(),
                        reason.clone(),
                        None,
                        ActivityFilter::All,
                    ),
                    AuditEvent::ReAnchor { look, reason, .. } => (
                        format!("{look} received new reference data."),
                        reason.clone(),
                        None,
                        ActivityFilter::All,
                    ),
                    AuditEvent::Revert { look, reason, .. } => (
                        format!("{look} returned to an earlier version."),
                        reason.clone(),
                        None,
                        ActivityFilter::All,
                    ),
                    AuditEvent::Split { detail, .. } => (
                        "Camera data was separated for review.".to_owned(),
                        detail.clone(),
                        None,
                        ActivityFilter::All,
                    ),
                };
                ActivityItem {
                    sentence,
                    reason,
                    proposal,
                    detail: event_sentence(&entry.event),
                    time: entry.ts.to_string(),
                    seq: entry.seq as i64,
                    category,
                }
            })
            .collect());
    }
    let mut items = Vec::new();
    let mut after = 0;
    loop {
        let response = gloo_net::http::Request::get(&crate::base::api_url("/log.json"))
            .query([("after", after.to_string()), ("limit", "200".to_owned())])
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.ok() {
            return Err("Activity could not be loaded. Try again later.".to_owned());
        }
        let page: LivePage = response.json().await.map_err(|error| error.to_string())?;
        items.extend(page.entries.into_iter().map(|entry| {
            let payload = entry.payload;
            let detail = format!(
                "Actor: {} · Subject: {} · Proposal: {}",
                payload
                    .actor
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("not recorded"),
                payload
                    .subject
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("not recorded"),
                payload
                    .proposal
                    .as_ref()
                    .map(|id| id.as_str())
                    .unwrap_or("not recorded")
            );
            let actor = payload
                .actor_handle
                .as_ref()
                .map(|handle| handle.as_str())
                .unwrap_or("A moderator");
            let subject = "a contributor";
            let look = payload
                .look
                .map(|look| look.to_string())
                .unwrap_or_else(|| "A look".to_owned());
            let (sentence, category) = match payload.event {
                LiveEventKind::GrantCreated => (
                    format!("{actor} granted a community role to {subject}."),
                    ActivityFilter::Grants,
                ),
                LiveEventKind::GrantRevoked => (
                    format!("{actor} removed a community role from {subject}."),
                    ActivityFilter::Revocations,
                ),
                LiveEventKind::EndorsementIssued => (
                    format!("{actor} vouched for {subject}."),
                    ActivityFilter::Endorsements,
                ),
                LiveEventKind::EndorsementRevoked | LiveEventKind::EndorsementSuperseded => (
                    format!("The endorsement for {subject} changed."),
                    ActivityFilter::Endorsements,
                ),
                LiveEventKind::ModerationDecision => (
                    format!(
                        "{actor} {} a proposal{}",
                        payload
                            .decision
                            .map(LiveDecision::wording)
                            .unwrap_or("reviewed"),
                        payload
                            .summary
                            .as_ref()
                            .map(|summary| format!(": {}", summary.headline()))
                            .unwrap_or_else(|| ".".to_owned())
                    ),
                    ActivityFilter::Decisions,
                ),
                LiveEventKind::ProposalOpened => (
                    payload
                        .summary
                        .as_ref()
                        .map(|summary| {
                            format!("A proposal opened for review: {}", summary.headline())
                        })
                        .unwrap_or_else(|| "A proposal opened for review.".to_owned()),
                    ActivityFilter::All,
                ),
                LiveEventKind::RootAdmitted => (
                    format!("{actor} accepted reference data for {look}."),
                    ActivityFilter::All,
                ),
                LiveEventKind::RootRevoked => (
                    format!("{actor} removed reference data for {look}."),
                    ActivityFilter::All,
                ),
                LiveEventKind::ReAnchor => (
                    format!("{look} received new reference data."),
                    ActivityFilter::All,
                ),
                LiveEventKind::Revert => (
                    format!("{look} returned to an earlier version."),
                    ActivityFilter::All,
                ),
                LiveEventKind::Split => (
                    format!("{look} separated into distinct looks."),
                    ActivityFilter::All,
                ),
                LiveEventKind::ReleaseManifest => {
                    (format!("{look} published."), ActivityFilter::Releases)
                }
                LiveEventKind::Unknown => (
                    "Community activity recorded.".to_owned(),
                    ActivityFilter::All,
                ),
            };
            ActivityItem {
                sentence,
                reason: payload.reason,
                detail,
                proposal: payload.proposal,
                time: ui::timestamp(entry.ts),
                seq: entry.seq,
                category,
            }
        }));
        match page.next_after {
            Some(next) if next > after => after = next,
            _ => break,
        }
    }
    Ok(items)
}
