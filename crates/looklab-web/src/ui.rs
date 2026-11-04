// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small presentational helpers shared by the pages.

use leptos::prelude::*;
use looklab_publish::{GateCheck, LookStatus};

pub fn loading() -> impl IntoView {
    view! { <p class="muted" role="status">"Loading…"</p> }
}

pub fn failure(message: &str) -> impl IntoView + use<> {
    view! {
        <div class="error" role="alert">
            <strong>"Could not load this page."</strong>
            <p>{message.to_owned()}</p>
        </div>
    }
}

#[component]
pub fn StatusBadge(status: LookStatus) -> impl IntoView {
    let label = status.as_str();
    view! { <span class=format!("badge badge-{label}")>{label}</span> }
}

#[component]
pub fn Gates(checks: Vec<GateCheck>) -> impl IntoView {
    view! {
        <table class="data-table">
            <caption>"Release gates"</caption>
            <thead>
                <tr><th scope="col">"Gate"</th><th scope="col">"Result"</th><th scope="col">"Detail"</th></tr>
            </thead>
            <tbody>
                {checks
                    .into_iter()
                    .map(|check| {
                        view! {
                            <tr>
                                <th scope="row">{check.name}</th>
                                <td>
                                    {if check.passed {
                                        view! { <span class="pass">"passed"</span> }.into_any()
                                    } else {
                                        view! { <span class="fail">"failed"</span> }.into_any()
                                    }}
                                </td>
                                <td>{check.detail.unwrap_or_default()}</td>
                            </tr>
                        }
                    })
                    .collect_view()}
            </tbody>
        </table>
    }
}

/// A `looklab-report` SVG fragment, with the report's palette and labels.
#[component]
pub fn Plot(title: String, svg: String) -> impl IntoView {
    view! {
        <figure class="plot">
            <figcaption>{title}</figcaption>
            <div class="plot-body" inner_html=svg></div>
        </figure>
    }
}
