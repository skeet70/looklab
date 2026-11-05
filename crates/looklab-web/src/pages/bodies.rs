// SPDX-License-Identifier: AGPL-3.0-or-later
use super::looks::{cards, photo};
use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, SiteIndex};
use std::sync::Arc;

#[component]
pub fn BodiesPage() -> impl IntoView {
    let state = RwSignal::new(Load::<(SiteIndex, Arc<PreparedScene>)>::Loading);
    let filter = RwSignal::new(String::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(
                match async { Ok((data::index().await?, photo().await?)) }.await {
                    Ok(value) => Load::Ready(value),
                    Err(error) => Load::Failed(error),
                },
            );
        });
    });
    view! {
        <h1>"Explore cameras"</h1>
        <p class="lede">"Discover the looks built into your camera, or borrow color from another."</p>
        <label for="body-filter">"Filter by make or model"</label>
        <input id="body-filter" type="search" placeholder="Fujifilm"
            on:input=move |event| filter.set(event_target_value(&event))/>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready((index, scene)) => {
                let items = cards(&index);
                let needle = filter.get().to_lowercase();
                view! {
                    <div class="camera-gallery">
                        {index.bodies.iter()
                            .filter(|body| body.kind == looklab_publish::BodyKind::Camera)
                            .filter(|body| body.make.to_lowercase().contains(&needle) || body.model.to_lowercase().contains(&needle))
                            .map(|body| view! {
                                <article class="camera-card">
                                    <div class="camera-look-strip">
                                        {items.iter().filter(|item| body.looks.contains(&item.slug))
                                            .take(2).cloned().map(|item| view! {
                                                <ui::LookCard item scene=scene.clone()/>
                                            }).collect_view()}
                                    </div>
                                    <h2>
                                        <a href=crate::base::site_url(&format!("/cameras/{}", body.id))>
                                            {format!("{} {}", body.make, body.model)}
                                        </a>
                                    </h2>
                                    <p>{format!("{} {} to explore", body.looks.len(), if body.looks.len() == 1 { "look" } else { "looks" })}</p>
                                </article>
                            }).collect_view()}
                    </div>
                }.into_any()
            }
        }}
    }
}
