// SPDX-License-Identifier: AGPL-3.0-or-later
//! The moderation queue and signed decisions ([docs/08 §6](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_schema::ProposalRef;

use crate::data::{self, Decision, DecisionReceipt, Load, ModeratorProposal, QueueProposal};
use crate::ui;

#[component]
pub fn ModQueuePage() -> impl IntoView {
    let session = RwSignal::new(data::moderation_session_open());
    let queue = RwSignal::new(Load::<Vec<QueueProposal>>::Loading);
    let selected = RwSignal::new(None::<ModeratorProposal>);
    let selected_id = RwSignal::new(None::<ProposalRef>);
    let reason = RwSignal::new(String::new());
    let receipt = RwSignal::new(None::<DecisionReceipt>);
    let notice = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let message = RwSignal::new(String::new());

    let refresh = move || {
        spawn_local(async move {
            queue.set(match data::moderation_queue().await {
                Ok(queue) => Load::Ready(queue),
                Err(error) => Load::Failed(error),
            });
        });
    };

    Effect::new(move |_| {
        if session.get() {
            refresh();
        }
    });

    let open_session = move |_| {
        busy.set(true);
        notice.set(None);
        spawn_local(async move {
            match data::open_moderation_session().await {
                Ok(()) => session.set(true),
                Err(error) => notice.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let review = move |id: ProposalRef| {
        selected_id.set(Some(id.clone()));
        selected.set(None);
        receipt.set(None);
        notice.set(None);
        spawn_local(async move {
            match data::moderator_proposal(&id).await {
                Ok(proposal) => selected.set(Some(proposal)),
                Err(error) => notice.set(Some(error)),
            }
        });
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
                    selected.set(None);
                    selected_id.set(None);
                    refresh();
                }
                Err(error) => notice.set(Some(error)),
            }
            busy.set(false);
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
                Ok(proposal) => selected.set(Some(proposal)),
                Err(error) => notice.set(Some(error)),
            }
            busy.set(false);
        });
    };

    view! {
        <h1>"Moderation queue"</h1>
        <p>
            "Each item shows its menu-level summary and evidence. Approve, reject or request
            information with a required reason; every decision is written to the audit log."
        </p>
        <section aria-labelledby="session">
            <h2 id="session">"Moderator session"</h2>
            <p>
                "Moderators work with a short-lived session opened by a passkey assertion. The
                queue and decisions are served only to that session."
            </p>
            {move || {
                if session.get() {
                    view! { <p class="muted" id="mod-session">"Session open."</p> }.into_any()
                } else {
                    view! {
                        <button
                            type="button"
                            id="mod-open-session"
                            disabled=move || busy.get()
                            on:click=open_session
                        >
                            "Open moderator session"
                        </button>
                    }
                        .into_any()
                }
            }}
            {move || {
                notice
                    .get()
                    .map(|text| view! { <p class="error" role="alert">{text}</p> })
            }}
        </section>
        {move || {
            if !session.get() {
                return ().into_any();
            }
            match queue.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(queue) if queue.is_empty() => {
                    view! { <p class="muted">"Nothing is waiting for moderation."</p> }.into_any()
                }
                Load::Ready(queue) => {
                    let rows = queue
                        .into_iter()
                        .map(|proposal| {
                            let id = proposal.id.clone();
                            let button_id = id.clone();
                            view! {
                                <tr>
                                    <th scope="row">
                                        <a href=format!("/proposals/{}", id.as_str())>
                                            {proposal.kind.menu_label()}
                                        </a>
                                    </th>
                                    <td>{proposal.summary.clone()}</td>
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
                        <table class="data-table" id="mod-queue">
                            <caption>"Open proposals"</caption>
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
                    view! {
                        <section aria-labelledby="mod-proposal">
                            <h2 id="mod-proposal">{selected.proposal.summary.headline.clone()}</h2>
                            <p class="muted">
                                {format!(
                                    "{} · {} evidence item(s)",
                                    selected.proposal.kind.menu_label(),
                                    selected.proposal.evidence.len(),
                                )}
                            </p>
                            <h3>"Payload"</h3>
                            <pre class="json" id="mod-payload">{selected.payload.clone()}</pre>
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
                            <label for="mod-reason">"Reason (required)"</label>
                            <textarea
                                id="mod-reason"
                                rows="3"
                                cols="50"
                                prop:value=move || reason.get()
                                on:input=move |event| reason.set(event_target_value(&event))
                            ></textarea>
                            <div class="field-row">
                                <button
                                    type="button"
                                    id="mod-approve"
                                    disabled=move || busy.get()
                                    on:click=move |_| decide(Decision::Approve)
                                >
                                    "Approve"
                                </button>
                                <button
                                    type="button"
                                    id="mod-reject"
                                    disabled=move || busy.get()
                                    on:click=move |_| decide(Decision::Reject)
                                >
                                    "Reject"
                                </button>
                                <button
                                    type="button"
                                    id="mod-request-info"
                                    disabled=move || busy.get()
                                    on:click=move |_| decide(Decision::RequestInfo)
                                >
                                    "Request info"
                                </button>
                            </div>
                            <p class="muted">{format!("Proposal {}", id.as_str())}</p>
                        </section>
                    }
                })
        }}
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
    }
}
