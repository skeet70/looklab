// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_publish::AuditLog;

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn LogPage() -> impl IntoView {
    let state = RwSignal::new(Load::<AuditLog>::Loading);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::audit_log().await {
                Ok(log) => Load::Ready(log),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <h1>"Audit log"</h1>
        <p>
            "Append-only. Entries reference credentials by internal id or display handle, never by public key or serial HMAC."
        </p>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(log) => {
                let json = serde_json::to_string_pretty(&log).unwrap_or_default();
                let rows = log
                    .entries
                    .iter()
                    .map(|entry| {
                        view! {
                            <tr>
                                <td>{entry.seq}</td>
                                <td>{entry.ts.to_string()}</td>
                                <td>{entry.event.kind()}</td>
                                <td><code>{serde_json::to_string(&entry.event).unwrap_or_default()}</code></td>
                            </tr>
                        }
                    })
                    .collect_view();
                view! {
                    <table class="data-table">
                        <caption>"Audit entries"</caption>
                        <thead>
                            <tr>
                                <th scope="col">"Seq"</th>
                                <th scope="col">"Time"</th>
                                <th scope="col">"Type"</th>
                                <th scope="col">"Event"</th>
                            </tr>
                        </thead>
                        <tbody>{rows}</tbody>
                    </table>
                    <details>
                        <summary>"Raw JSON"</summary>
                        <pre class="json">{json}</pre>
                    </details>
                }
                .into_any()
            }
        }}
    }
}
