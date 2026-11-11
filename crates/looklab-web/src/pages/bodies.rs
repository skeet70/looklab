// SPDX-License-Identifier: AGPL-3.0-or-later
use super::looks::cards;
use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, SceneKind, SiteIndex};
use std::sync::Arc;

#[derive(Clone, Debug)]
struct BodiesReady {
    index: SiteIndex,
    scene: Option<Arc<PreparedScene>>,
    preview_error: Option<String>,
}

#[component]
pub fn BodiesPage() -> impl IntoView {
    let state = RwSignal::new(Load::<BodiesReady>::Loading);
    let filter = RwSignal::new(String::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(
                match async {
                    let index = data::index().await?;
                    let photo = index
                        .scenes
                        .iter()
                        .find(|scene| scene.kind == SceneKind::Photo)
                        .map(|scene| scene.name.clone());
                    let (scene, preview_error) = match photo {
                        Some(name) => match data::scene_thumbnail(name.as_str()).await {
                            Ok(loaded) => (Some(Arc::new(loaded)), None),
                            Err(error) => (None, Some(error)),
                        },
                        None => (None, None),
                    };
                    Ok(BodiesReady {
                        index,
                        scene,
                        preview_error,
                    })
                }
                .await
                {
                    Ok(value) => Load::Ready(value),
                    Err(error) => Load::Failed(error),
                },
            );
        });
    });
    view! {
        <h1>"Cameras"</h1>
        <p class="lede">"Pick a camera to see its built-in looks and what the evidence covers."</p>
        <label for="body-filter">"Filter by make or model"</label>
        <input id="body-filter" type="search" placeholder="Fujifilm"
            on:input=move |event| filter.set(event_target_value(&event))/>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(BodiesReady { index, scene, preview_error }) => {
                let items = cards(&index);
                let needle = filter.get().to_lowercase();
                let documented = index
                    .manuals
                    .iter()
                    .filter(|manual| !index.bodies.iter().any(|body| body.id == manual.id))
                    .filter(|manual| manual.make.to_lowercase().contains(&needle) || manual.model.to_lowercase().contains(&needle))
                    .cloned()
                    .collect::<Vec<_>>();
                view! {
                    {preview_error.map(|error| view! {
                        <p class="muted" role="status">"Look previews could not load. Camera details are still available."</p>
                        <ui::Disclosure summary="Preview details".to_owned()><p>{error}</p></ui::Disclosure>
                    })}
                    <div class="camera-gallery">
                        {index.bodies.iter()
                            .filter(|body| body.kind == looklab_publish::BodyKind::Camera)
                            .filter(|body| body.make.to_lowercase().contains(&needle) || body.model.to_lowercase().contains(&needle))
                            .map(|body| {
                                let scene = scene.clone();
                                let published = items.iter().filter(|item| body.looks.contains(&item.slug));
                                let published_count = published.clone().count();
                                view! {
                                    <article class="camera-card">
                                        <div class="camera-look-strip">
                                            {published.take(2).cloned().map(|item| match &scene {
                                                    Some(scene) => view! { <ui::LookCard item scene=scene.clone()/> }.into_any(),
                                                    None => view! { <ui::LookCard item/> }.into_any(),
                                                }).collect_view()}
                                        </div>
                                        <h2>
                                            <a href=crate::base::site_url(&format!("/cameras/{}", body.id))>
                                                {format!("{} {}", body.make, body.model)}
                                            </a>
                                        </h2>
                                        <p>{if published_count == 0 {
                                            "No measured looks published yet".to_owned()
                                        } else {
                                            format!("{published_count} {} to explore", if published_count == 1 { "look" } else { "looks" })
                                        }}</p>
                                        {index.manuals.iter().any(|manual| manual.id == body.id).then(|| view! {
                                            <p><span class="badge">"Documented settings available"</span></p>
                                        })}
                                    </article>
                                }
                            }).collect_view()}
                    </div>
                    {(!documented.is_empty()).then(|| view! {
                        <section aria-labelledby="documented-cameras">
                            <h2 id="documented-cameras">"Documented cameras"</h2>
                            <p class="lede">"Official manuals list controls for these cameras. Owner confirmation is shown per control, and contribution support may not be available yet."</p>
                            <div class="camera-gallery">
                                {documented.into_iter().map(|manual| view! {
                                    <article class="camera-card">
                                        <h2>
                                            <a href=crate::base::site_url(&format!("/cameras/{}", manual.id))>
                                                {format!("{} {}", manual.make, manual.model)}
                                            </a>
                                        </h2>
                                        <p><span class="badge">"From official documentation · owner checks per control"</span></p>
                                        <p>{format!(
                                            "{} documented controls · {} sources · {} confirmed · {} contradicted",
                                            manual.documented_controls,
                                            manual.source_count,
                                            manual.confirmed_controls,
                                            manual.contradicted_controls,
                                        )}</p>
                                        <p class="muted">{if manual.preparation.is_ready() { "Stock-settings model resources published" } else { "Stock-settings model resources not published yet" }}</p>
                                    </article>
                                }).collect_view()}
                            </div>
                        </section>
                    })}
                }.into_any()
            }
        }}
    }
}
