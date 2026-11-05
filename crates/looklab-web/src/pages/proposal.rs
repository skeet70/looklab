// SPDX-License-Identifier: AGPL-3.0-or-later
//! One proposal, its evidence, confirmations and private thread ([docs/08 §4 §6](../../../docs/08-ui.md)).

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use looklab_schema::{Evidence, ProposalRef, ProposalStatus, ProposalView};

use crate::data::{self, Load, MessageAuthor, ProposalMessage};
use crate::proposal_form;
use crate::ui;

#[component]
pub fn ProposalDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<ProposalView>::Loading);
    let id = RwSignal::new(None::<ProposalRef>);

    Effect::new(move |_| {
        let text = params.read().get("id").unwrap_or_default();
        let Ok(proposal) = ProposalRef::try_from(text) else {
            state.set(Load::Failed(
                "the proposal id in the address is not valid".to_owned(),
            ));
            return;
        };
        id.set(Some(proposal.clone()));
        spawn_local(async move {
            state.set(match data::proposal(&proposal).await {
                Ok(proposal) => Load::Ready(proposal),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => {
                view! {
                    <h1>"Proposal"</h1>
                    <p class="muted">
                        "Proposals live in the server's moderation queue, not in the static site data."
                    </p>
                    {ui::failure(&error)}
                }
                .into_any()
            }
            Load::Ready(proposal) => {
                match id.get() {
                    Some(id) => {
                        view! { <ProposalViewDetail proposal id state/> }.into_any()
                    }
                    None => ui::failure("the proposal id is missing").into_any(),
                }
            }
        }}
    }
}

#[component]
fn ProposalViewDetail(
    proposal: ProposalView,
    id: ProposalRef,
    state: RwSignal<Load<ProposalView>>,
) -> impl IntoView {
    let kind = proposal.kind;
    let status = proposal.status;
    let created = ui::timestamp(proposal.created_at);
    let details = proposal
        .summary
        .details
        .iter()
        .map(|detail| view! { <li>{detail.clone()}</li> })
        .collect_view();
    let evidence = proposal
        .evidence
        .iter()
        .map(|item| view! { <li>{describe(item)}</li> })
        .collect_view();

    let confirm_label = RwSignal::new("provia".to_owned());
    let confirm_tags = RwSignal::new("FilmMode=0x0000".to_owned());
    let confirm_message = RwSignal::new(None::<String>);

    let confirm_id = id.clone();
    let confirm = move |_| {
        let current = confirm_id.clone();
        match proposal_form::parse_tag_values(&confirm_tags.get(), &confirm_label.get()) {
            Err(error) => confirm_message.set(Some(error)),
            Ok(tags) => {
                confirm_message.set(None);
                spawn_local(async move {
                    match data::confirm_proposal(&current, &tags).await {
                        Ok(view) => {
                            state.set(Load::Ready(view));
                            confirm_message
                                .set(Some("Confirmation recorded as evidence.".to_owned()));
                        }
                        Err(error) => confirm_message.set(Some(error)),
                    }
                });
            }
        }
    };

    let thread = RwSignal::new(None::<Vec<ProposalMessage>>);
    let thread_message = RwSignal::new(String::new());
    let thread_notice = RwSignal::new(None::<String>);
    let thread_busy = RwSignal::new(false);

    let load_id = id.clone();
    let load_thread = move |_| {
        let current = load_id.clone();
        thread_busy.set(true);
        thread_notice.set(None);
        spawn_local(async move {
            match data::proposer_thread(&current).await {
                Ok(messages) => thread.set(Some(messages)),
                Err(error) => thread_notice.set(Some(error)),
            }
            thread_busy.set(false);
        });
    };

    let send_id = id.clone();
    let send_thread = move |_| {
        let body = thread_message.get().trim().to_owned();
        if body.is_empty() {
            thread_notice.set(Some("Write a message first.".to_owned()));
            return;
        }
        let current = send_id.clone();
        thread_busy.set(true);
        thread_notice.set(None);
        spawn_local(async move {
            match data::proposer_message(&current, &body).await {
                Ok(()) => {
                    thread_message.set(String::new());
                    match data::proposer_thread(&current).await {
                        Ok(messages) => thread.set(Some(messages)),
                        Err(error) => thread_notice.set(Some(error)),
                    }
                }
                Err(error) => thread_notice.set(Some(error)),
            }
            thread_busy.set(false);
        });
    };

    view! {
        <h1>{proposal.summary.headline.clone()}</h1>
        <p class="muted">
            {format!("{} · {} · proposed {created}", kind.menu_label(), status_label(status))}
        </p>
        <ul>{details}</ul>
        <section aria-labelledby="evidence">
            <h2 id="evidence">"Evidence"</h2>
            <p>
                "Confirmations are evidence, not votes: each one attaches the confirmer's own parsed,
                whitelisted tag values."
            </p>
            <ul>{evidence}</ul>
        </section>
        <section aria-labelledby="confirm">
            <h2 id="confirm">"Confirm this menu"</h2>
            <p>
                "If your camera shows this menu too, your confirmation attaches the parsed tag values
                from your own file. It needs a passkey signature."
            </p>
            <label for="confirm-label">"Which menu answer did your file show?"</label>
            <input
                type="text"
                id="confirm-label"
                prop:value=move || confirm_label.get()
                on:input=move |event| confirm_label.set(event_target_value(&event))
            />
            <label for="confirm-tags">"Parsed tag values, one Tag=value per line"</label>
            <textarea
                id="confirm-tags"
                rows="4"
                cols="40"
                prop:value=move || confirm_tags.get()
                on:input=move |event| confirm_tags.set(event_target_value(&event))
            ></textarea>
            <button type="button" id="confirm-proposal" on:click=confirm>
                "Confirm with passkey"
            </button>
            {move || {
                confirm_message
                    .get()
                    .map(|text| view! { <p class="muted" role="status">{text}</p> })
            }}
        </section>
        <section aria-labelledby="thread">
            <h2 id="thread">"Discussion"</h2>
            <p class="muted">
                "Discussion is limited to the proposer and moderators. A moderator can request more
                information; there is no open forum."
            </p>
            {if kind == looklab_schema::ProposalKind::InterfaceProfile {
                view! {
                    <p>
                        "A round-trip report from the pipeline simulator was generated at submission
                        and is visible to moderators."
                    </p>
                }
                    .into_any()
            } else {
                ().into_any()
            }}
            <button type="button" id="load-thread" disabled=move || thread_busy.get() on:click=load_thread>
                "Load discussion"
            </button>
            {move || {
                thread
                    .get()
                    .map(|messages| {
                        if messages.is_empty() {
                            view! { <p class="muted">"No messages yet."</p> }.into_any()
                        } else {
                            view! {
                                <ul id="thread-messages">
                                    {messages
                                        .into_iter()
                                        .map(|message| {
                                            let author = match message.author {
                                                MessageAuthor::Proposer => "Proposer",
                                                MessageAuthor::Moderator => "Moderator",
                                            };
                                            let handle = message
                                                .handle
                                                .map(|handle| format!(" ({handle})"))
                                                .unwrap_or_default();
                                            view! {
                                                <li>
                                                    <strong>{format!("{author}{handle}")}</strong>
                                                    {format!(": {}", message.body)}
                                                </li>
                                            }
                                        })
                                        .collect_view()}
                                </ul>
                            }
                            .into_any()
                        }
                    })
            }}
            <label for="thread-body">"Message to the moderators"</label>
            <textarea
                id="thread-body"
                rows="3"
                cols="50"
                prop:value=move || thread_message.get()
                on:input=move |event| thread_message.set(event_target_value(&event))
            ></textarea>
            <button type="button" id="send-thread" disabled=move || thread_busy.get() on:click=send_thread>
                "Send message"
            </button>
            {move || {
                thread_notice
                    .get()
                    .map(|text| view! { <p class="error" role="alert">{text}</p> })
            }}
        </section>
    }
}

fn status_label(status: ProposalStatus) -> &'static str {
    match status {
        ProposalStatus::Pending => "awaiting moderation",
        ProposalStatus::NeedsInfo => "more information requested",
        ProposalStatus::Approved => "approved",
        ProposalStatus::Rejected => "rejected",
    }
}

fn describe(evidence: &Evidence) -> String {
    match evidence {
        Evidence::Confirmation {
            author,
            tags,
            recorded_at,
        } => {
            let values = tags
                .iter()
                .map(|tag| format!("{}={:04x} ({})", tag.tag, tag.value.0, tag.label))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "Confirmed by {} on {recorded_at} with {values}",
                author.as_deref().unwrap_or("a contributor")
            )
        }
        Evidence::Fixture { donation } => format!(
            "Fixture donation: {} ({}, {})",
            donation.file,
            donation.license.as_str(),
            donation.donated_on
        ),
        Evidence::Worker { summary, .. } => format!("Worker evidence: {summary}"),
    }
}
