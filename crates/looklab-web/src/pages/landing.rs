// SPDX-License-Identifier: AGPL-3.0-or-later

use super::looks::{cards, photo};
use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, SiteIndex};
use std::sync::Arc;

/// What the home page can offer from the published catalog.
#[derive(Clone, Debug)]
enum HomeReady {
    Choices {
        index: SiteIndex,
        scene: Arc<PreparedScene>,
    },
    Collecting,
}

#[component]
pub fn Landing() -> impl IntoView {
    let state = RwSignal::new(Load::<HomeReady>::Loading);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let loaded = async {
                let index = data::index().await?;
                if cards(&index).is_empty() {
                    return Ok(HomeReady::Collecting);
                }
                Ok(HomeReady::Choices {
                    index,
                    scene: photo().await?,
                })
            }
            .await;
            state.set(match loaded {
                Ok(ready) => Load::Ready(ready),
                Err(error) => Load::Failed(error),
            });
        });
    });
    view! {
        <section class="home-heading">
            <p class="eyebrow">"A world of color. Your camera."</p>
            <h1>{move || match state.get() {
                Load::Ready(HomeReady::Collecting) => "Camera looks, measured from photos shared by the people who shoot them.",
                _ => "See any camera's look on your photos, then get it on your camera.",
            }}</h1>
        </section>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(HomeReady::Collecting) => view! { <Collecting/> }.into_any(),
            Load::Ready(HomeReady::Choices { index, scene }) => view! { <HomeChoices index scene/> }.into_any(),
        }}
        <Show when=move || state.with(|state| !matches!(state, Load::Ready(HomeReady::Collecting)))>
            <section class="home-help"><div><h2>"Built together, photo by photo"</h2><p>"Shared photos help us understand each camera's color. About a minute with photos you already have."</p></div><a class="button" href=crate::base::site_url("/contribute")>"Help build looks"</a></section>
        </Show>
        <section class="home-developers"><p>"Building camera firmware? Give your users a new world of looks."</p><a href=crate::base::site_url("/developers")>"For developers →"</a></section>
    }
}

#[component]
fn Collecting() -> impl IntoView {
    crate::prefetch::route_rendered();
    view! {
        <section class="home-collecting">
            <h2>"No looks are published yet"</h2>
            <p>"looklab measures a camera's color from RAW and JPEG pairs shared by the people who shoot it. Looks are published once a camera has enough checked photos."</p>
            <p class="field-row">
                <a class="button" href=crate::base::site_url("/contribute")>"Share photos you already have"</a>
                <a href=crate::base::site_url("/about/how-it-works")>"How looks are made →"</a>
            </p>
        </section>
    }
}

#[component]
fn HomeChoices(index: SiteIndex, scene: Arc<PreparedScene>) -> impl IntoView {
    let items = cards(&index);
    let camera = RwSignal::new(index.bodies.first().map(|camera| camera.id.clone()));
    let look = RwSignal::new(items.first().map(|item| item.slug.clone()));
    let show_reference_labels = index.reference_preview.is_some();
    let initial_right = if index.reference_preview.is_some() {
        items.first()
    } else {
        items.get(1).or_else(|| items.first())
    };
    let right = RwSignal::new(initial_right.cloned());
    let preview = RwSignal::new(Load::<(Arc<Vec<u8>>, Arc<Vec<u8>>, String, String)>::Loading);
    let first = items.first().cloned();
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let first = first.clone();
        let right = right.get();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        wasm_bindgen_futures::spawn_local(async move {
            let result = async {
                let b = right.ok_or_else(|| "Choose a look.".to_owned())?;
                let (a_lut, a_name) = match data::reference_preview().await? {
                    Some(lut) => (lut, "Neutral".to_owned()),
                    None => {
                        let a = first.ok_or_else(|| "No looks published yet.".to_owned())?;
                        (
                            data::preview(a.slug.as_str(), a.version.as_str()).await?,
                            a.name,
                        )
                    }
                };
                Ok((
                    a_lut,
                    data::preview(b.slug.as_str(), b.version.as_str()).await?,
                    a_name,
                    b.name,
                ))
            }
            .await;
            if generation.get_untracked() != mine {
                return;
            }
            preview.set(match result {
                Ok(value) => Load::Ready(value),
                Err(error) => Load::Failed(error),
            });
        });
    });
    let hero_scene = RwSignal::new(scene.clone());
    let hero_name = scene.scene.name.to_string();
    let upgrade_started = StoredValue::new(false);
    let gallery_ready = RwSignal::new(false);
    Effect::new(move |_| {
        if preview.with(|state| matches!(state, Load::Failed(_))) {
            gallery_ready.set(true);
            return;
        }
        if !preview.with(|state| matches!(state, Load::Ready(_))) || upgrade_started.get_value() {
            return;
        }
        upgrade_started.set_value(true);
        let name = hero_name.clone();
        wasm_bindgen_futures::spawn_local(async move {
            ui::paint_browser().await;
            gallery_ready.set(true);
            if let Ok(photo) = data::scene(&name).await {
                hero_scene.set(Arc::new(photo));
            }
        });
    });
    let picker_scene = scene.clone();
    let picker_items = items.clone();
    let chips = items.clone();
    view! {
        <section class="home-preview">
            {move || match preview.get() {
                Load::Ready((a, b, an, bn)) => view! {
                    <div class="home-preview-labels" hidden=!show_reference_labels>
                        <span>{an.clone()}</span>
                        <span>{bn.clone()}</span>
                    </div>
                    <ui::Viewer lut_a=Signal::stored(a) lut_b=Signal::stored(b)
                        scene=Signal::derive(move || hero_scene.get()) label_a=an label_b=bn controls=false/>
                }.into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Loading => ui::loading().into_any(),
            }}
            <div class="look-chips" aria-label="Preview a look">
                {chips.into_iter().map(|item| {
                    let selected = item.slug.clone();
                    view! {
                        <button type="button"
                            aria-pressed=move || right.get().is_some_and(|item| item.slug == selected).to_string()
                            on:click=move |_| right.set(Some(item.clone()))>
                            {item.name.clone()}
                        </button>
                    }
                }).collect_view()}
            </div>
        </section>
        <section class="home-picker"><h2>"Make it yours"</h2><div class="home-picker-grid">
            <ui::CameraPicker cameras=index.bodies selected=camera label="What do you shoot?".to_owned()/>
            <div><h3>"Which look?"</h3><ui::LookPicker items=picker_items scene=picker_scene on_select=Callback::new(move |item: ui::LookCardItem| look.set(Some(item.slug))) label="Choose a look".to_owned()/></div>
            <a class="button" href=move || crate::base::site_url(&format!("/match?camera={}&look={}", camera.get().map(|id| id.to_string()).unwrap_or_default(), look.get().map(|slug| slug.to_string()).unwrap_or_default()))>"Continue →"</a>
        </div></section>
        <section>
            <div class="section-heading">
                <h2>"Find a color that feels like you"</h2>
                <a href=crate::base::site_url("/looks")>"Explore all looks →"</a>
            </div>
            <div class="look-gallery">
                <Show when=move || gallery_ready.get()>
                    {items.clone().into_iter().map(|item| view! {
                        <super::looks::CharacterCard item scene=scene.clone()/>
                    }).collect_view()}
                </Show>
            </div>
        </section>
    }
}
