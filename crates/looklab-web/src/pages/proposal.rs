// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use looklab_schema::{Evidence, ProposalStatus, ProposalView};

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn ProposalDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<ProposalView>::Loading);
    Effect::new(move |_| {
        let id = params.read().get("id").unwrap_or_default();
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::proposal(&id).await {
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
            Load::Ready(proposal) => view! { <ProposalViewDetail proposal/> }.into_any(),
        }}
    }
}

#[component]
fn ProposalViewDetail(proposal: ProposalView) -> impl IntoView {
    let kind = proposal.kind;
    let status = proposal.status;
    let created = timestamp(proposal.created_at);
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
            <button type="button" disabled=true>"Confirm with passkey"</button>
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
        } => format!(
            "Confirmed by {} on {recorded_at} with {} parsed tag value(s)",
            author.as_deref().unwrap_or("a contributor"),
            tags.len()
        ),
        Evidence::Fixture { donation } => format!(
            "Fixture donation: {} ({}, {})",
            donation.file,
            donation.license.as_str(),
            donation.donated_on
        ),
        Evidence::Worker { summary, .. } => format!("Worker evidence: {summary}"),
    }
}

fn timestamp(seconds: i64) -> String {
    let millis = seconds as f64 * 1000.0;
    let date = js_sys::Date::new(&millis.into());
    String::from(date.to_iso_string())
}
