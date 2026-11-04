// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_publish::SiteIndex;

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn LooksPage() -> impl IntoView {
    let state = RwSignal::new(Load::<SiteIndex>::Loading);
    let filter = RwSignal::new(String::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <h1>"Looks"</h1>
        <p>"Native families and LUT-defined looks. A published version pins every artifact by hash."</p>
        <label for="look-filter">"Filter by name"</label>
        <input
            id="look-filter"
            type="search"
            placeholder="Classic Chrome"
            on:input=move |event| filter.set(event_target_value(&event))
        />
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(index) => {
                let needle = filter.get().to_lowercase();
                let cards = index
                    .looks
                    .into_iter()
                    .filter(|look| {
                        needle.is_empty()
                            || look.name.to_lowercase().contains(&needle)
                            || look.slug.as_str().contains(&needle)
                    })
                    .map(|look| {
                        let version = look.latest().map(|version| version.version.to_string());
                        view! {
                            <li class="card">
                                <h2>
                                    <a href=format!("/looks/{}", look.slug)>{look.name.clone()}</a>
                                </h2>
                                <p class="muted">{look.slug.to_string()}</p>
                                <p>
                                    {match look.kind {
                                        looklab_model::file::LookKind::NativeFamily => "Native family",
                                        looklab_model::file::LookKind::LutDefined => "LUT-defined",
                                    }}
                                </p>
                                <p>
                                    {version
                                        .map(|version| format!("latest version {version}"))
                                        .unwrap_or_else(|| "no published version".to_owned())}
                                </p>
                            </li>
                        }
                    })
                    .collect_view();
                view! { <ul class="card-list">{cards}</ul> }.into_any()
            }
        }}
    }
}
