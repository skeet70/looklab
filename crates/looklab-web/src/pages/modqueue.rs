// SPDX-License-Identifier: AGPL-3.0-or-later
//! The moderation queue and signed decisions ([docs/08 §6](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::components::A;
use looklab_schema::{ManualCheckReviewStatus, ManualOwnerLinkage, ProposalRef};

use crate::data::{self, Decision, DecisionReceipt, Load, ModeratorProposal, ReviewQueue};
use crate::ui;

#[path = "review_actions.rs"]
mod actions;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReviewTab {
    Queue,
    Endorsements,
    Grants,
    Log,
}

impl ReviewTab {
    fn from_id(id: &ui::TabId) -> Self {
        match id.as_str() {
            "endorsements" => Self::Endorsements,
            "grants" => Self::Grants,
            "log" => Self::Log,
            _ => Self::Queue,
        }
    }
    fn id(self) -> ui::TabId {
        ui::TabId::new(match self {
            Self::Queue => "queue",
            Self::Endorsements => "endorsements",
            Self::Grants => "grants",
            Self::Log => "log",
        })
        .expect("static tab id")
    }
    fn label(self) -> &'static str {
        match self {
            Self::Queue => "Queue",
            Self::Endorsements => "Endorsements",
            Self::Grants => "Grants",
            Self::Log => "Log",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ReviewTarget {
    Proposal(ProposalRef),
    FailedGate(usize),
    FitHold(usize),
    SerialReuse(usize),
    ManualCheck(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum QueueFilter {
    All,
    Proposal(looklab_schema::ProposalKind),
    FailedGates,
    FitHolds,
    SharedCameras,
    CameraChecks,
}

impl QueueFilter {
    fn parse(value: &str) -> Self {
        match value {
            "failed-gates" => Self::FailedGates,
            "fit-holds" => Self::FitHolds,
            "shared-cameras" => Self::SharedCameras,
            "camera-checks" => Self::CameraChecks,
            _ => value.parse().map(Self::Proposal).unwrap_or(Self::All),
        }
    }
    fn matches(self, kind: looklab_schema::ProposalKind) -> bool {
        self == Self::All || self == Self::Proposal(kind)
    }
}

#[component]
pub fn ModQueuePage() -> impl IntoView {
    let session = RwSignal::new(data::moderation_session_open());
    let queue = RwSignal::new(Load::<ReviewQueue>::Loading);
    let selected = RwSignal::new(None::<ModeratorProposal>);
    let selected_proposal = selected;
    let selected_id = RwSignal::new(None::<ProposalRef>);
    let selected_alert = RwSignal::new(None::<ReviewTarget>);
    let reason = RwSignal::new(String::new());
    let receipt = RwSignal::new(None::<DecisionReceipt>);
    let notice = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let message = RwSignal::new(String::new());
    let available = crate::base::server_available();
    let profile = crate::session::use_session();
    let dialog = RwSignal::new(false);
    let pending_decision = RwSignal::new(Decision::Approve);
    let look_slug = RwSignal::new(String::new());
    let listing_notice = RwSignal::new(None::<String>);
    let revert_slug = RwSignal::new(String::new());
    let revert_version = RwSignal::new(String::new());
    let revert_rejected = RwSignal::new(String::new());
    let revert_reason = RwSignal::new(String::new());
    let revert_notice = RwSignal::new(None::<String>);
    let block_credential = RwSignal::new(String::new());
    let block_reason = RwSignal::new(String::new());
    let block_notice = RwSignal::new(None::<String>);
    let reanchor_slug = RwSignal::new(String::new());
    let reanchor_version = RwSignal::new(String::new());
    let reanchor_reason = RwSignal::new(String::new());
    let reanchor_notice = RwSignal::new(None::<String>);
    let filter = RwSignal::new(QueueFilter::parse(
        leptos_router::hooks::use_query_map()
            .get_untracked()
            .get("filter")
            .unwrap_or_default()
            .as_str(),
    ));
    let expiry_timer = StoredValue::new(None::<leptos::leptos_dom::helpers::TimeoutHandle>);
    on_cleanup(move || {
        if let Some(timer) = expiry_timer.get_value() {
            timer.clear();
        }
    });
    let tab_ui = RwSignal::new(ReviewTab::Queue.id());
    let tab = Memo::new(move |_| ReviewTab::from_id(&tab_ui.get()));
    let can_moderate = move || matches!(profile.state.get(), crate::session::SessionState::SignedIn(session) if session.roles.iter().any(|role| matches!(role, crate::webauthn::SessionRole::Moderator | crate::webauthn::SessionRole::Admin)));
    let tabs = move || {
        let mut tabs = Vec::new();
        if can_moderate() {
            tabs.push(ReviewTab::Queue);
        }
        if let crate::session::SessionState::SignedIn(session) = profile.state.get() {
            if session.tier == crate::webauthn::SessionTier::Seeder {
                tabs.push(ReviewTab::Endorsements);
            }
            if session.roles.contains(&crate::webauthn::SessionRole::Admin) {
                tabs.push(ReviewTab::Grants);
            }
        }
        tabs.push(ReviewTab::Log);
        tabs.into_iter()
            .map(|tab| ui::TabSpec {
                id: tab.id(),
                label: tab.label().to_owned(),
            })
            .collect::<Vec<_>>()
    };
    Effect::new(move |_| {
        if !can_moderate()
            && matches!(profile.state.get(), crate::session::SessionState::SignedIn(session) if session.tier == crate::webauthn::SessionTier::Seeder)
        {
            tab_ui.set(ReviewTab::Endorsements.id());
        }
    });

    let refresh = move || {
        spawn_local(async move {
            queue.set(match data::moderation_queue().await {
                Ok(queue) => Load::Ready(queue),
                Err(error) => Load::Failed(error),
            });
        });
    };

    Effect::new(move |_| {
        if let Some(timer) = expiry_timer.get_value() {
            timer.clear();
            expiry_timer.set_value(None);
        }
        if session.get() {
            refresh();
            if let Some(remaining) = data::moderation_session_remaining() {
                let timer = set_timeout_with_handle(
                    move || {
                        if !data::moderation_session_open() {
                            session.set(false);
                            queue.set(Load::Loading);
                            selected.set(None);
                            selected_id.set(None);
                            selected_alert.set(None);
                            dialog.set(false);
                            notice.set(Some(
                                "Your review session ended. Confirm it's you to continue."
                                    .to_owned(),
                            ));
                        }
                    },
                    remaining,
                );
                match timer {
                    Ok(timer) => expiry_timer.set_value(Some(timer)),
                    Err(_) => notice.set(Some(
                        "This browser could not schedule the review-session timeout.".to_owned(),
                    )),
                }
            }
        }
    });
    Effect::new(move |_| {
        notice.track();
        queue.track();
        profile.state.track();
        if session.get() && !data::moderation_session_open() {
            session.set(false);
            queue.set(Load::Loading);
            selected.set(None);
            selected_id.set(None);
            selected_alert.set(None);
            dialog.set(false);
        }
    });

    let open_session = move |_| {
        busy.set(true);
        notice.set(None);
        spawn_local(async move {
            match data::open_moderation_session(profile).await {
                Ok(_) => session.set(true),
                Err(error) => notice.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let review = move |id: ProposalRef| {
        selected_alert.set(None);
        selected_id.set(Some(id.clone()));
        selected.set(None);
        receipt.set(None);
        notice.set(None);
        spawn_local(async move {
            match data::moderator_proposal(&id).await {
                Ok(proposal) if selected_id.get().as_ref() == Some(&id) => {
                    selected.set(Some(proposal))
                }
                Err(error) if selected_id.get().as_ref() == Some(&id) => notice.set(Some(error)),
                _ => {}
            }
        });
    };
    let activate = move |target: ReviewTarget| match target {
        ReviewTarget::Proposal(id) => review(id),
        target => {
            selected.set(None);
            selected_id.set(None);
            selected_alert.set(Some(target));
        }
    };

    let decide = move |decision: Decision| {
        let Some(id) = selected_id.get() else {
            notice.set(Some("Choose a queue item first.".to_owned()));
            return;
        };
        let reason = reason.get().trim().to_owned();
        if reason.is_empty() {
            notice.set(Some("A reason is required for every decision.".to_owned()));
            return;
        }
        busy.set(true);
        notice.set(None);
        receipt.set(None);
        spawn_local(async move {
            match data::decide(&id, decision, &reason).await {
                Ok(outcome) => {
                    receipt.set(Some(outcome));
                    dialog.set(false);
                    selected.set(None);
                    selected_id.set(None);
                    refresh();
                }
                Err(error) => notice.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let moderate_listing = move |action: data::LookListingAction| {
        let look = look_slug.get_untracked().trim().to_owned();
        if look.is_empty() {
            listing_notice.set(Some("Enter the Look slug.".to_owned()));
            return;
        }
        let reason = reason.get_untracked().trim().to_owned();
        if reason.is_empty() {
            listing_notice.set(Some("A reason is required.".to_owned()));
            return;
        }
        busy.set(true);
        listing_notice.set(None);
        spawn_local(async move {
            let outcome = data::set_look_listing(action, &look, &reason).await;
            busy.set(false);
            listing_notice.set(Some(match outcome {
                Ok(()) => format!(
                    "{} {look}.",
                    match action {
                        data::LookListingAction::Hide => "Hidden",
                        data::LookListingAction::Remove => "Removed",
                        data::LookListingAction::Relist => "Relisted",
                    }
                ),
                Err(error) => error,
            }));
        });
    };

    let revert_look = move |_| {
        let look = revert_slug.get_untracked().trim().to_owned();
        if look.is_empty() {
            revert_notice.set(Some("Enter the Look slug.".to_owned()));
            return;
        }
        let version = match revert_version.get_untracked().trim().parse::<u32>() {
            Ok(version) if version > 0 => version,
            _ => {
                revert_notice.set(Some(
                    "Enter the published version number to restore.".to_owned(),
                ));
                return;
            }
        };
        let reason = revert_reason.get_untracked().trim().to_owned();
        if reason.is_empty() {
            revert_notice.set(Some("A reason is required.".to_owned()));
            return;
        }
        let rejected: Vec<String> = revert_rejected
            .get_untracked()
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .collect();
        busy.set(true);
        revert_notice.set(None);
        spawn_local(async move {
            let outcome = data::revert_look(&look, version, &rejected, &reason).await;
            busy.set(false);
            revert_notice.set(Some(match outcome {
                Ok(receipt) => {
                    let restoration = match receipt.restoration {
                        data::Restoration::Current => "the current target",
                        data::Restoration::HistoricalNeedsRebuild { .. } => {
                            "a historical copy that needs rebuilding"
                        }
                    };
                    format!(
                        "Reverted {look} to version {} and published version {} as {restoration}.",
                        receipt.target_version, receipt.new_version
                    )
                }
                Err(error) => error,
            }));
        });
    };

    let block_intake = move |_| {
        let credential = block_credential.get_untracked().trim().to_owned();
        if credential.is_empty() {
            block_notice.set(Some("Enter the profile's internal ID.".to_owned()));
            return;
        }
        let reason = block_reason.get_untracked().trim().to_owned();
        if reason.is_empty() {
            block_notice.set(Some("A reason is required.".to_owned()));
            return;
        }
        busy.set(true);
        block_notice.set(None);
        spawn_local(async move {
            let outcome = data::block_intake(&credential, &reason).await;
            busy.set(false);
            block_notice.set(Some(match outcome {
                Ok(receipt) => format!(
                    "Blocked {} from new contributions. Withdrawal and status access remain available.",
                    receipt.credential
                ),
                Err(error) => error,
            }));
        });
    };

    let reanchor_look = move |_| {
        let look = reanchor_slug.get_untracked().trim().to_owned();
        if look.is_empty() {
            reanchor_notice.set(Some("Enter the Look slug.".to_owned()));
            return;
        }
        let version = match reanchor_version.get_untracked().trim().parse::<u32>() {
            Ok(version) if version > 0 => version,
            _ => {
                reanchor_notice.set(Some(
                    "Enter the released version number to anchor.".to_owned(),
                ));
                return;
            }
        };
        let reason = reanchor_reason.get_untracked().trim().to_owned();
        if reason.is_empty() {
            reanchor_notice.set(Some("A reason is required.".to_owned()));
            return;
        }
        busy.set(true);
        reanchor_notice.set(None);
        spawn_local(async move {
            let outcome = data::reanchor_look(&look, version, &reason).await;
            busy.set(false);
            reanchor_notice.set(Some(match outcome {
                Ok(receipt) => format!("Anchored {} at version {}.", receipt.look, receipt.version),
                Err(error) => error,
            }));
        });
    };

    let send_message = move |_| {
        let Some(id) = selected_id.get() else {
            return;
        };
        let body = message.get().trim().to_owned();
        if body.is_empty() {
            notice.set(Some("Write a message first.".to_owned()));
            return;
        }
        busy.set(true);
        notice.set(None);
        spawn_local(async move {
            if let Err(error) = data::moderator_message(&id, &body).await {
                notice.set(Some(error));
                busy.set(false);
                return;
            }
            message.set(String::new());
            match data::moderator_proposal(&id).await {
                Ok(proposal) if selected_id.get().as_ref() == Some(&id) => {
                    selected.set(Some(proposal))
                }
                Err(error) if selected_id.get().as_ref() == Some(&id) => notice.set(Some(error)),
                _ => {}
            }
            busy.set(false);
        });
    };

    view! {
        <div class="review-page">
        <Show when=move || profile.can_review() fallback=|| view! {
            <h1>"Review queue"</h1>
            <p>"This area is for community moderators."</p>
            <a href=crate::base::site_url("/community")>"Back to Community"</a>
        }>
        <h1>"Review queue"</h1>
        <p class="lede">"Check the evidence. Explain the decision. Help keep camera data useful."</p>
        {(!available).then(ui::preview_notice)}
        <section class="review-session" aria-labelledby="session" hidden=move || !available || !can_moderate()>
            <h2 id="session">"Confirm it's you"</h2>
            <p class="muted">"Use your passkey once to open a short-lived review session."</p>
            <div class="field-row">
                {move || {
                    if session.get() {
                        view! { <p class="muted" id="mod-session">"Session open."</p> }.into_any()
                    } else {
                        view! {
                            <button
                                type="button"
                                id="mod-open-session"
                                disabled=move || busy.get() || !available
                                on:click=open_session
                            >
                                "Confirm it's you"
                            </button>
                        }
                            .into_any()
                    }
                }}
            </div>
            {move || {
                notice
                    .get()
                    .map(|text| view! { <p class="error" role="alert">{text}</p> })
            }}
        </section>
        <div class="review-look-actions" hidden=move || !available || !can_moderate() || !session.get()>
            <ui::Disclosure summary="Look actions".to_owned()>
                <div class="review-maintenance">
                    <section aria-labelledby="look-listing">
                        <h2 id="look-listing">"Look listing"</h2>
                        <p class="muted">"Hide or remove a LUT Look that a native Look supersedes, or relist one."</p>
                        <div class="review-form-grid">
                            <label for="mod-look-slug">"Look slug"
                                <input id="mod-look-slug" type="text" on:input=move |event| look_slug.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-look-reason">"Reason"
                                <input id="mod-look-reason" type="text" on:input=move |event| reason.set(event_target_value(&event))/>
                            </label>
                        </div>
                        <div class="field-row">
                            <button type="button" id="mod-look-hide" disabled=move || busy.get() on:click=move |_| moderate_listing(data::LookListingAction::Hide)>"Hide"</button>
                            <button type="button" id="mod-look-remove" disabled=move || busy.get() on:click=move |_| moderate_listing(data::LookListingAction::Remove)>"Remove"</button>
                            <button type="button" id="mod-look-relist" disabled=move || busy.get() on:click=move |_| moderate_listing(data::LookListingAction::Relist)>"Relist"</button>
                        </div>
                        {move || listing_notice.get().map(|text| view! { <p class="muted" role="status">{text}</p> })}
                    </section>
                    <section aria-labelledby="look-revert">
                        <h2 id="look-revert">"Revert a Look"</h2>
                        <p class="muted">"Publish a released version's content as a new version and reject the named contributions. Other contributions from the same profile keep their weight."</p>
                        <div class="review-form-grid">
                            <label for="mod-revert-slug">"Look slug"
                                <input id="mod-revert-slug" type="text" on:input=move |event| revert_slug.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-revert-version">"Version to restore"
                                <input id="mod-revert-version" type="number" min="1" on:input=move |event| revert_version.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-revert-rejected">"Reject contributions (comma separated internal IDs)"
                                <input id="mod-revert-rejected" type="text" on:input=move |event| revert_rejected.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-revert-reason">"Reason"
                                <input id="mod-revert-reason" type="text" on:input=move |event| revert_reason.set(event_target_value(&event))/>
                            </label>
                        </div>
                        <div class="field-row">
                            <button type="button" id="mod-revert-submit" disabled=move || busy.get() on:click=revert_look>"Revert and reject"</button>
                        </div>
                        {move || revert_notice.get().map(|text| view! { <p class="muted" role="status">{text}</p> })}
                    </section>
                    <section aria-labelledby="profile-block">
                        <h2 id="profile-block">"Block future contributions"</h2>
                        <p class="muted">"Denies new photo uploads, manual confirmations, proposals and fixture donations from one profile. Existing data, withdrawal, status access and sign-in stay available."</p>
                        <div class="review-form-grid">
                            <label for="mod-block-credential">"Profile internal ID"
                                <input id="mod-block-credential" type="text" on:input=move |event| block_credential.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-block-reason">"Reason"
                                <input id="mod-block-reason" type="text" on:input=move |event| block_reason.set(event_target_value(&event))/>
                            </label>
                        </div>
                        <div class="field-row">
                            <button type="button" id="mod-block-submit" disabled=move || busy.get() on:click=block_intake>"Block future contributions"</button>
                        </div>
                        {move || block_notice.get().map(|text| view! { <p class="muted" role="status">{text}</p> })}
                    </section>
                    <section aria-labelledby="look-reanchor">
                        <h2 id="look-reanchor">"Re-anchor a Look"</h2>
                        <p class="muted">"Start the cumulative release gate at a released version. The action is logged with its reason."</p>
                        <div class="review-form-grid">
                            <label for="mod-reanchor-slug">"Look slug"
                                <input id="mod-reanchor-slug" type="text" on:input=move |event| reanchor_slug.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-reanchor-version">"Released version"
                                <input id="mod-reanchor-version" type="number" min="1" on:input=move |event| reanchor_version.set(event_target_value(&event))/>
                            </label>
                            <label for="mod-reanchor-reason">"Reason"
                                <input id="mod-reanchor-reason" type="text" on:input=move |event| reanchor_reason.set(event_target_value(&event))/>
                            </label>
                        </div>
                        <div class="field-row">
                            <button type="button" id="mod-reanchor-submit" disabled=move || busy.get() on:click=reanchor_look>"Re-anchor"</button>
                        </div>
                        {move || reanchor_notice.get().map(|text| view! { <p class="muted" role="status">{text}</p> })}
                    </section>
                </div>
            </ui::Disclosure>
        </div>
        <ui::Tabs tabs=tabs() selected=tab_ui>
        <div tabindex="0" aria-label="Proposal queue, use j and k to navigate" hidden=move || tab.get() != ReviewTab::Queue on:keydown=move |event| {
            if event.target().and_then(|target| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(target).ok()).is_some_and(|element| matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT")) { return; }
            let direction = match event.key().as_str() { "j" => 1isize, "k" => -1, _ => return };
            if let Load::Ready(items) = queue.get() {
                let items = review_targets(&items, filter.get());
                if items.is_empty() { return; }
                let active = selected_id.get().map(ReviewTarget::Proposal).or_else(|| selected_alert.get());
                let current = items.iter().position(|item| Some(item) == active.as_ref());
                let next = current.map_or(0, |index| (index as isize + direction).clamp(0, items.len() as isize - 1) as usize);
                event.prevent_default();
                activate(items[next].clone());
            }
        }>
        <div class="field-row review-filters">
            <label for="review-filter">"Item type"</label>
            <select id="review-filter" on:change=move |event| filter.set(QueueFilter::parse(&event_target_value(&event)))>
                <option value="" selected=move || filter.get() == QueueFilter::All>"All types"</option>
                {looklab_schema::ProposalKind::ALL.into_iter().map(|kind| view! {
                    <option value=kind.as_str() selected=move || filter.get() == QueueFilter::Proposal(kind)>{move || format!("{} ({})", crate::proposal_form::kind_label(kind), match queue.get() {
                        Load::Ready(items) => items.proposals.iter().filter(|item| item.kind == kind).count(),
                        _ => 0,
                    })}</option>
                }).collect_view()}
                <option value="failed-gates" selected=move || filter.get() == QueueFilter::FailedGates>{move || format!("Release checks ({})", match queue.get() {
                    Load::Ready(items) => items.failed_gates.len(), _ => 0,
                })}</option>
                <option value="fit-holds" selected=move || filter.get() == QueueFilter::FitHolds>{move || format!("Fit not converging ({})", match queue.get() {
                    Load::Ready(items) => items.fit_holds.len(), _ => 0,
                })}</option>
                <option value="shared-cameras" selected=move || filter.get() == QueueFilter::SharedCameras>{move || format!("Shared camera flags ({})", match queue.get() {
                    Load::Ready(items) => items.serial_reuse.len(), _ => 0,
                })}</option>
                <option value="camera-checks" selected=move || filter.get() == QueueFilter::CameraChecks>{move || format!("Camera checks ({})", match queue.get() {
                    Load::Ready(items) => items.manual_checks.len(), _ => 0,
                })}</option>
            </select>
            <p class="muted">"Use j and k to move through items."</p>
        </div>
        <div class="review-panes" class:has-selection=move || selected_id.get().is_some() || selected_alert.get().is_some()>
        {move || {
            if !session.get() {
                return ().into_any();
            }
            match queue.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(queue) if queue.proposals.is_empty() && queue.failed_gates.is_empty() && queue.fit_holds.is_empty() && queue.serial_reuse.is_empty() && queue.manual_checks.is_empty() => {
                    view! { <p class="muted">"Nothing is waiting for moderation."</p> }.into_any()
                }
                Load::Ready(queue) => {
                    let count = review_targets(&queue, filter.get()).len();
                    let rows = queue.proposals
                        .into_iter()
                        .filter(|item| filter.get().matches(item.kind))
                        .map(|proposal| {
                            let id = proposal.id.clone();
                            let button_id = id.clone();
                            view! {
                                <tr>
                                    <th scope="row">
                                        <a href=crate::base::site_url(&format!("/proposals/{}", id.as_str()))>
                                            {crate::proposal_form::kind_label(proposal.kind)}
                                        </a>
                                    </th>
                                    <td>{crate::proposal_form::menu_summary(proposal.kind, &looklab_schema::ProposalSummary::new(proposal.summary.lines().next().unwrap_or_default())).headline}</td>
                                    <td>{ui::timestamp(proposal.created_at)}</td>
                                    <td>
                                        <button
                                            type="button"
                                            data-proposal=id.as_str().to_owned()
                                            on:click=move |_| review(button_id.clone())
                                        >
                                            "Review"
                                        </button>
                                    </td>
                                </tr>
                            }
                        })
                        .collect_view();
                    view! {
                        <div class="queue-list">
                        <table class="data-table" id="mod-queue">
                            <caption>{format!("{count} items to review")}</caption>
                            <thead>
                                <tr>
                                    <th scope="col">"Kind"</th>
                                    <th scope="col">"Summary"</th>
                                    <th scope="col">"Proposed at"</th>
                                    <th scope="col">"Actions"</th>
                                </tr>
                            </thead>
                            <tbody>{rows}</tbody>
                        </table>
                        <ul class="activity-list" hidden=move || !matches!(filter.get(), QueueFilter::All | QueueFilter::FailedGates)>
                            {queue.failed_gates.into_iter().enumerate().map(|(index, gate)| view! {
                                <li>
                                    <button type="button" on:click=move |_| activate(ReviewTarget::FailedGate(index))>
                                        {format!("Review {} version {} release checks", gate.look, gate.version.get())}
                                    </button>
                                    <small>{ui::timestamp(gate.created_at)}</small>
                                </li>
                            }).collect_view()}
                        </ul>
                        <ul class="activity-list" hidden=move || !matches!(filter.get(), QueueFilter::All | QueueFilter::FitHolds)>
                            {queue.fit_holds.into_iter().enumerate().map(|(index, hold)| view! {
                                <li>
                                    <button type="button" on:click=move |_| activate(ReviewTarget::FitHold(index))>
                                        {format!("Fit not converging: {} after {} batches", hold.look, hold.consecutive)}
                                    </button>
                                    <small>{ui::timestamp(hold.last_attempt)}</small>
                                </li>
                            }).collect_view()}
                        </ul>
                        <ul class="activity-list" hidden=move || !matches!(filter.get(), QueueFilter::All | QueueFilter::SharedCameras)>
                            {queue.serial_reuse.into_iter().enumerate().map(|(index, flag)| view! {
                                <li>
                                    <button type="button" on:click=move |_| activate(ReviewTarget::SerialReuse(index))>
                                        {format!("Review a camera shared by {} profiles", flag.credentials.len())}
                                    </button>
                                    <p>{format!("{} contributions need a check.", flag.contributions.len())}</p>
                                </li>
                            }).collect_view()}
                        </ul>
                        <ul class="activity-list" hidden=move || !matches!(filter.get(), QueueFilter::All | QueueFilter::CameraChecks)>
                            {queue.manual_checks.into_iter().enumerate().map(|(index, check)| {
                                let title = format!(
                                    "{} {}: {}",
                                    check.camera.make(),
                                    check.camera.model(),
                                    check.label
                                );
                                let detail = format!(
                                    "Firmware {} · {} · {}",
                                    check.firmware.as_str(),
                                    super::body::finding_text(&check.finding),
                                    manual_check_status_text(&check.status)
                                );
                                view! {
                                    <li>
                                        <button type="button" on:click=move |_| activate(ReviewTarget::ManualCheck(index))>
                                            {title}
                                        </button>
                                        <p>{detail}</p>
                                        <small>{ui::timestamp(check.submitted_at)}</small>
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                        </div>
                    }
                        .into_any()
                }
            }
        }}
        {move || {
            selected
                .get()
                .map(|selected| {
                    let id = selected.proposal.id.clone();
                    let summary = crate::proposal_form::menu_summary(selected.proposal.kind, &selected.proposal.summary);
                    view! {
                        <section aria-labelledby="mod-proposal">
                            <button type="button" class="review-back" on:click=move |_| { selected_id.set(None); selected_proposal.set(None); }>"Back to queue"</button>
                            <h2 id="mod-proposal">{summary.headline.clone()}</h2>
                            <p class="muted">
                                {format!(
                                    "{} · {} evidence item(s)",
                                    crate::proposal_form::kind_label(selected.proposal.kind),
                                    selected.proposal.evidence.len(),
                                )}
                            </p>
                            <h3 hidden=summary.details.is_empty()>"What changes"</h3>
                            <ul>{summary.details.iter().map(|detail| view! { <li>{detail.clone()}</li> }).collect_view()}</ul>
                            <h3>"Evidence"</h3>
                            <ul>{selected.proposal.evidence.iter().map(|item| view! { <li>{super::proposal::describe(item)}</li> }).collect_view()}</ul>
                            <ui::Disclosure summary="Technical details".to_owned()><pre class="json" id="mod-payload">{selected.payload.clone()}</pre></ui::Disclosure>
                            {selected
                                .round_trip
                                .map(|report| {
                                    view! {
                                        <p class="muted">
                                            {format!(
                                                "Round-trip report: {} probes, {} clipped target(s), {}.",
                                                report.probes,
                                                report.target_clipped,
                                                if report.flagged { "flagged" } else { "clean" },
                                            )}
                                        </p>
                                    }
                                })}
                            <h3>"Thread"</h3>
                            <ul id="mod-thread">
                                {selected
                                    .thread
                                    .iter()
                                    .map(|entry| {
                                        let author = match entry.author {
                                            data::MessageAuthor::Proposer => "Proposer",
                                            data::MessageAuthor::Moderator => "Moderator",
                                        };
                                        view! {
                                            <li>
                                                <strong>{author}</strong>
                                                {format!(": {}", entry.body)}
                                            </li>
                                        }
                                    })
                                    .collect_view()}
                            </ul>
                            <label for="mod-message">"Message to the proposer"</label>
                            <textarea
                                id="mod-message"
                                rows="3"
                                cols="50"
                                prop:value=move || message.get()
                                on:input=move |event| message.set(event_target_value(&event))
                            ></textarea>
                            <button
                                type="button"
                                id="mod-send-message"
                                disabled=move || busy.get()
                                on:click=send_message
                            >
                                "Send message"
                            </button>
                            <h3>"Decision"</h3>
                            <p>"Choose a decision to review its public record before sending."</p>
                            <div class="field-row">
                                <button
                                    type="button"
                                    id="mod-approve"
                                    disabled=move || busy.get()
                                    on:click=move |_| { pending_decision.set(Decision::Approve); dialog.set(true); }
                                >
                                    "Approve"
                                </button>
                                <button
                                    type="button"
                                    id="mod-reject"
                                    disabled=move || busy.get()
                                    on:click=move |_| { pending_decision.set(Decision::Reject); dialog.set(true); }
                                >
                                    "Reject"
                                </button>
                                <button
                                    type="button"
                                    id="mod-request-info"
                                    disabled=move || busy.get()
                                    on:click=move |_| { pending_decision.set(Decision::RequestInfo); dialog.set(true); }
                                >
                                    "Request info"
                                </button>
                            </div>
                            <p class="muted">{format!("Proposal {}", id.as_str())}</p>
                        </section>
                    }
                })
        }}
        {move || queue.with(|state| {
            let Load::Ready(queue) = state else { return ().into_any(); };
            match selected_alert.get() {
                Some(ReviewTarget::FailedGate(index)) => queue.failed_gates.get(index).map(|gate| {
                    let report = serde_json::to_string_pretty(&gate.report).unwrap_or_default();
                    view! {
                        <section>
                            <button type="button" class="review-back" on:click=move |_| selected_alert.set(None)>"Back to queue"</button>
                            <h2>{format!("{} needs a release check", gate.look)}</h2>
                            <p>{format!("Version {} has not passed its release checks. It has not been published.", gate.version.get())}</p>
                            <h3>"Evidence"</h3>
                            <ul>{gate.report.violations.iter().map(|violation| view! {
                                <li>{violation_summary(violation)}</li>
                            }).collect_view()}</ul>
                            <ui::Disclosure summary="Technical details".to_owned()>
                                <pre class="json">{report}</pre>
                            </ui::Disclosure>
                            <p>"Resolve the reported checks before publishing this version. Worker-generated release exceptions appear as proposals in this queue."</p>
                        </section>
                    }.into_any()
                }).unwrap_or_else(|| ().into_any()),
                Some(ReviewTarget::FitHold(index)) => queue.fit_holds.get(index).map(|hold| {
                    let diagnostics = serde_json::to_string_pretty(&hold.diagnostics).unwrap_or_default();
                    view! {
                        <section>
                            <button type="button" class="review-back" on:click=move |_| selected_alert.set(None)>"Back to queue"</button>
                            <h2>"Fit not converging"</h2>
                            <p>{format!("{} has not converged in {} consecutive batches. Nothing was published for those batches.", hold.look, hold.consecutive)}</p>
                            <p class="muted">{ui::timestamp(hold.first_attempt)}" to "{ui::timestamp(hold.last_attempt)}</p>
                            <h3>"Last diagnostics"</h3>
                            <pre class="json">{diagnostics}</pre>
                            <p>"The worker keeps resuming from the last state. This item resolves on the next published version."</p>
                        </section>
                    }.into_any()
                }).unwrap_or_else(|| ().into_any()),
                Some(ReviewTarget::SerialReuse(index)) => queue.serial_reuse.get(index).map(|flag| view! {
                    <section>
                        <button type="button" class="review-back" on:click=move |_| selected_alert.set(None)>"Back to queue"</button>
                        <h2>"One camera, several profiles"</h2>
                        <p>"These profiles shared files from the same camera. Check the evidence before changing trust or reference data."</p>
                        <h3>"Profiles"</h3>
                        <ul>{flag.credentials.iter().map(|id| view! { <li>{id.to_string()}</li> }).collect_view()}</ul>
                        <h3>"Contributions"</h3>
                        <ul>{flag.contributions.iter().map(|id| view! { <li>{id.to_string()}</li> }).collect_view()}</ul>
                        <p>"Sharing a camera is not evidence of misuse. No trust change happens automatically."</p>
                    </section>
                }.into_any()).unwrap_or_else(|| ().into_any()),
                Some(ReviewTarget::ManualCheck(index)) => queue.manual_checks.get(index).map(|check| {
                    let camera_path = format!(
                        "/cameras/{}?control={}#manual-moderation",
                        check.camera.as_str(),
                        check.control.as_str()
                    );
                    let definition = check.definition_digest.to_string();
                    let linkage = match &check.owner_linkage {
                        ManualOwnerLinkage::Linked => {
                            "Linked to this camera by a photo contribution."
                        }
                        ManualOwnerLinkage::Unlinked => {
                            "No photo contribution links this profile to the camera."
                        }
                    };
                    view! {
                        <section>
                            <button type="button" class="review-back" on:click=move |_| selected_alert.set(None)>"Back to queue"</button>
                            <h2>{format!("{} {}: {}", check.camera.make(), check.camera.model(), check.label)}</h2>
                            <p class="muted"><code>{check.control.as_str()}</code></p>
                            <dl class="manual-check-detail">
                                <dt>"Firmware"</dt>
                                <dd>{check.firmware.as_str().to_owned()}</dd>
                                <dt>"Submitted finding"</dt>
                                <dd>{super::body::finding_text(&check.finding)}</dd>
                                <dt>"Review status"</dt>
                                <dd>{manual_check_status_text(&check.status)}</dd>
                                <dt>"Submission"</dt>
                                <dd>"Latest active check for this control and firmware."</dd>
                                <dt>"Owner linkage"</dt>
                                <dd>{linkage}</dd>
                                <dt>"Submitted definition digest"</dt>
                                <dd><code>{definition}</code></dd>
                                <dt>"Submitted"</dt>
                                <dd>{ui::timestamp(check.submitted_at)}</dd>
                            </dl>
                            <p class="muted">"A moderator decision cites evidence for this control. It does not change the schema or enable RAW decoding or fitting."</p>
                            <p><A href=crate::base::site_url(&camera_path)>"Open this control's camera review"</A></p>
                        </section>
                    }
                }.into_any()).unwrap_or_else(|| ().into_any()),
                _ => ().into_any(),
            }
        })}
        </div></div>
        <div hidden=move || tab.get() != ReviewTab::Endorsements>
            <h2>"Endorsement nominations"</h2>
            <p>"These contributors have accepted work for a trusted member to review."</p>
            <p class="muted" hidden=can_moderate>"Nominations are available to profiles with review queue access. You can review a contributor by internal ID below."</p>
            {move || queue.with(|state| match state {
                Load::Ready(queue) => view! {
                    <ul class="activity-list">{queue.nominations.iter().map(|nomination| view! {
                        <li>
                            <strong>{nomination.display_handle.as_ref().map(|handle| handle.as_str().to_owned()).unwrap_or_else(|| "A contributor".to_owned())}</strong>
                            <p>{format!("{} accepted contributions", nomination.accepted_contributions.get())}</p>
                            <small>{nomination.credential.to_string()}</small>
                        </li>
                    }).collect_view()}</ul>
                    {queue.nominations.is_empty().then(|| view! {
                        <p class="muted">"No contributors are waiting for an endorsement review."</p>
                    })}
                }.into_any(),
                _ => ().into_any(),
            })}
            <actions::TrustActions seeder=true admin=false/>
            <super::log::PublicList endorsements=true/>
        </div>
        <div hidden=move || tab.get() != ReviewTab::Grants>
            <actions::TrustActions seeder=false admin=true/>
            <super::log::PublicList endorsements=false/>
        </div>
        <div hidden=move || tab.get() != ReviewTab::Log>
            <a href=crate::base::site_url("/community/log")>"Open the public audit log"</a>
        </div>
        </ui::Tabs>
        <ui::Dialog title="Review your decision".to_owned() open=dialog>
            <label for="mod-template">"Common reasons"</label>
            <select id="mod-template" on:change=move |event| reason.set(event_target_value(&event))>
                <option value="">"Choose a reason or write your own"</option>
                {move || {
                    let presets = match pending_decision.get() {
                        Decision::Approve => [
                            "The supplied evidence supports this change.",
                            "The schema matches the documented camera controls.",
                        ],
                        Decision::Reject => [
                            "The proposed change does not match the supplied evidence.",
                            "The proposed change is outside the supported scope.",
                        ],
                        Decision::RequestInfo => [
                            "Please provide evidence supporting the proposed change.",
                            "Please clarify the controls and firmware this change applies to.",
                        ],
                    };
                    presets.into_iter().map(|preset| view! {
                        <option>{preset}</option>
                    }).collect_view()
                }}
            </select>
            <label for="mod-reason">"Reason (required)"</label>
            <textarea id="mod-reason" rows="3" prop:value=move || reason.get()
                on:input=move |event| reason.set(event_target_value(&event))></textarea>
            <h3>"Public audit-log preview"</h3>
            <p id="mod-log-preview">{move || format!("Moderator {} proposal {}: {}", match pending_decision.get() {
                Decision::Approve => "approved",
                Decision::Reject => "rejected",
                Decision::RequestInfo => "requested information on",
            }, selected_id.get().map(|id| id.as_str().to_owned()).unwrap_or_default(), reason.get())}</p>
            <button type="button" id="mod-confirm-decision" disabled=move || busy.get() || reason.get().trim().is_empty()
                on:click=move |_| decide(pending_decision.get())>"Submit decision"</button>
        </ui::Dialog>
        {move || {
            receipt
                .get()
                .map(|receipt| {
                    let version = receipt
                        .snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.version.to_string())
                        .unwrap_or_default();
                    let text = match &receipt.snapshot {
                        Some(snapshot) => format!(
                            "Decision recorded. Snapshot v{} with {} file(s), content hash {}.",
                            snapshot.version, snapshot.files, snapshot.content_hash
                        ),
                        None => "Decision recorded. This proposal kind publishes no snapshot.".to_owned(),
                    };
                    view! {
                        <p
                            id="mod-decision"
                            class="valid"
                            role="status"
                            data-snapshot-version=version
                        >
                            {text}
                        </p>
                    }
                })
        }}
        </Show>
        </div>
    }
}

fn review_targets(queue: &ReviewQueue, filter: QueueFilter) -> Vec<ReviewTarget> {
    let mut targets: Vec<_> = queue
        .proposals
        .iter()
        .filter(|proposal| filter.matches(proposal.kind))
        .map(|proposal| ReviewTarget::Proposal(proposal.id.clone()))
        .collect();
    if matches!(filter, QueueFilter::All | QueueFilter::FailedGates) {
        targets.extend((0..queue.failed_gates.len()).map(ReviewTarget::FailedGate));
    }
    if matches!(filter, QueueFilter::All | QueueFilter::FitHolds) {
        targets.extend((0..queue.fit_holds.len()).map(ReviewTarget::FitHold));
    }
    if matches!(filter, QueueFilter::All | QueueFilter::SharedCameras) {
        targets.extend((0..queue.serial_reuse.len()).map(ReviewTarget::SerialReuse));
    }
    if matches!(filter, QueueFilter::All | QueueFilter::CameraChecks) {
        targets.extend((0..queue.manual_checks.len()).map(ReviewTarget::ManualCheck));
    }
    targets
}

fn manual_check_status_text(status: &ManualCheckReviewStatus) -> &'static str {
    match status {
        ManualCheckReviewStatus::Counted => "counted toward agreement",
        ManualCheckReviewStatus::Pending => "pending review",
        ManualCheckReviewStatus::StaleDefinition => "older control definition",
        ManualCheckReviewStatus::Reviewed => "reviewed by moderator evidence",
    }
}

fn violation_summary(violation: &looklab_trust::Violation) -> &'static str {
    use looklab_trust::Violation;
    match violation {
        Violation::DuplicateProbe { .. } => "A measurement appears more than once.",
        Violation::MissingProbe { .. } => "A required measurement is missing.",
        Violation::RootMedian { .. } | Violation::RootP95 { .. } => {
            "The fit moved too far from the reference photos."
        }
        Violation::OverCeiling { .. }
        | Violation::OverBound { .. }
        | Violation::TooManyOverBound { .. }
        | Violation::P95Ratio { .. } => "The look changed more than the release limits allow.",
        Violation::PreClipOverCeiling { .. } => {
            "Brightness changed more than the release limits allow."
        }
        Violation::HueOverCeiling { .. } => "Hue changed more than the release limits allow.",
        Violation::SupportedToUnsupported { .. } => {
            "A previously measured region is no longer supported."
        }
    }
}
