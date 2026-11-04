// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_publish::SiteIndex;

use crate::data::{self, Load};
use crate::ui;

#[component]
pub fn Landing() -> impl IntoView {
    let state = RwSignal::new(Load::<SiteIndex>::Loading);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        <section class="hero">
            <h1>"Measure, compare and reproduce camera looks"</h1>
            <p class="lede">
                "Pick a body, then a look. Everything here is static data: no account and no upload."
            </p>
        </section>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(index) => view! { <LandingChoices index/> }.into_any(),
        }}
    }
}

#[component]
fn LandingChoices(index: SiteIndex) -> impl IntoView {
    let first_body = index
        .bodies
        .first()
        .map(|body| body.id.to_string())
        .unwrap_or_default();
    let first_look = index
        .looks
        .first()
        .map(|look| look.slug.to_string())
        .unwrap_or_default();
    let body = RwSignal::new(first_body);
    let look = RwSignal::new(first_look);

    let body_link = {
        let index = index.clone();
        move || {
            let selected = body.get();
            index
                .bodies
                .iter()
                .find(|entry| entry.id.to_string() == selected)
                .map(|entry| format!("/bodies/{}", entry.id))
                .unwrap_or_else(|| "/bodies".to_owned())
        }
    };
    let look_link = move || format!("/looks/{}", look.get());
    let match_link = move || format!("/match?look={}", look.get());

    view! {
        <div class="picker-grid">
            <section class="panel" aria-labelledby="pick-body">
                <h2 id="pick-body">"1. Your body"</h2>
                <label for="body-select">"Body"</label>
                <select
                    id="body-select"
                    on:change=move |event| body.set(event_target_value(&event))
                >
                    {index
                        .bodies
                        .iter()
                        .map(|entry| {
                            view! {
                                <option value=entry.id.to_string()>
                                    {format!("{} {}", entry.make, entry.model)}
                                </option>
                            }
                        })
                        .collect_view()}
                </select>
                <p><a href=body_link>"Open the body page"</a></p>
            </section>
            <section class="panel" aria-labelledby="pick-look">
                <h2 id="pick-look">"2. A look"</h2>
                <label for="look-select">"Look"</label>
                <select
                    id="look-select"
                    on:change=move |event| look.set(event_target_value(&event))
                >
                    {index
                        .looks
                        .iter()
                        .map(|entry| {
                            view! { <option value=entry.slug.to_string()>{entry.name.clone()}</option> }
                        })
                        .collect_view()}
                </select>
                <p><a href=look_link>"Open the look page"</a></p>
            </section>
            <section class="panel" aria-labelledby="pick-match">
                <h2 id="pick-match">"3. Match"</h2>
                <p>"Settings and artifact matching arrive with the browser match flow."</p>
                <p><a class="button" href=match_link>"Match this look (coming soon)"</a></p>
            </section>
        </div>
        <section aria-labelledby="browse">
            <h2 id="browse">"Browse"</h2>
            <ul class="card-list">
                <li><a href="/bodies">"All bodies"</a>" - " {index.bodies.len()} " indexed"</li>
                <li><a href="/looks">"All looks"</a>" - " {index.looks.len()} " indexed"</li>
                <li><a href="/compare">"Compare two looks"</a>" - diagnostics, ΔE and region maps"</li>
                <li><a href="/log">"Audit log"</a>" - grants, endorsements, decisions and releases"</li>
            </ul>
        </section>
    }
}
