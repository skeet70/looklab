// SPDX-License-Identifier: AGPL-3.0-or-later
//! Small presentational helpers shared by the pages.

use leptos::prelude::*;
use looklab_publish::{GateCheck, LookStatus};

#[path = "components/mod.rs"]
pub mod components;
pub use components::*;

#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export function looklabPaint() {
    return new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = looklabPaint)]
    fn paint_frame() -> js_sys::Promise;
}

pub(crate) async fn paint_browser() {
    let _ = wasm_bindgen_futures::JsFuture::from(paint_frame()).await;
}

pub fn camera_kind_label(kind: looklab_publish::BodyKind) -> &'static str {
    match kind {
        looklab_publish::BodyKind::Camera => "Camera",
        looklab_publish::BodyKind::Software => "Editing app",
    }
}

pub fn mapping_status_label(status: looklab_publish::MappingStatus) -> &'static str {
    match status {
        looklab_publish::MappingStatus::Provisional => "Not yet confirmed from a sample file",
        looklab_publish::MappingStatus::FixtureBacked => "Confirmed from a sample file",
    }
}

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

/// The notice on pages whose actions need the API.
pub fn preview_notice() -> impl IntoView + use<> {
    view! {
        <div class="preview-notice" role="note">
            <strong>"Sharing is turned off in this demo."</strong>
            <p>"You can still explore looks and process your photos on this device."</p>
        </div>
    }
}

pub fn timestamp(seconds: i64) -> String {
    let millis = seconds as f64 * 1000.0;
    let date = js_sys::Date::new(&millis.into());
    String::from(date.to_iso_string())
}

#[component]
pub fn StatusBadge(status: LookStatus) -> impl IntoView {
    view! { <Badge status/> }
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
