// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_schema::ProposalView;

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn ModQueuePage() -> impl IntoView {
    let state = RwSignal::new(Load::<Vec<ProposalView>>::Loading);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::proposal_queue().await {
                Ok(queue) => Load::Ready(queue),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <h1>"Moderation queue"</h1>
        <p>
            "Each item shows its menu-level summary and evidence. Approve, reject or request
            information with a required reason; every decision is written to the audit log."
        </p>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => {
                view! {
                    <p class="muted">
                        "This build ships no proposals bundle. The queue is served by the intake API
                        to a moderator passkey session."
                    </p>
                    {ui::failure(&error)}
                }
                .into_any()
            }
            Load::Ready(queue) if queue.is_empty() => {
                view! { <p class="muted">"Nothing is waiting for moderation."</p> }.into_any()
            }
            Load::Ready(queue) => {
                let rows = queue
                    .iter()
                    .map(|proposal| {
                        let id = proposal.id.as_str().to_owned();
                        view! {
                            <tr>
                                <th scope="row">
                                    <a href=format!("/proposals/{id}")>{proposal.kind.menu_label()}</a>
                                </th>
                                <td>{proposal.summary.headline.clone()}</td>
                                <td>{proposal.evidence.len().to_string()}</td>
                                <td>
                                    <button type="button" disabled=true>"Approve"</button>
                                    <button type="button" disabled=true>"Reject"</button>
                                    <button type="button" disabled=true>"Request info"</button>
                                </td>
                            </tr>
                        }
                    })
                    .collect_view();
                view! {
                    <table class="data-table">
                        <caption>"Open proposals"</caption>
                        <thead>
                            <tr>
                                <th scope="col">"Kind"</th>
                                <th scope="col">"Summary"</th>
                                <th scope="col">"Evidence"</th>
                                <th scope="col">"Actions"</th>
                            </tr>
                        </thead>
                        <tbody>{rows}</tbody>
                    </table>
                    <p class="muted">
                        "Actions open a short-lived session with a passkey assertion; this static
                        build cannot sign."
                    </p>
                }
                .into_any()
            }
        }}
    }
}
