// SPDX-License-Identifier: AGPL-3.0-or-later

use super::looks::{cards, photo};
use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, SiteIndex};
use std::sync::Arc;

#[component]
pub fn Landing() -> impl IntoView {
    let state = RwSignal::new(Load::<(SiteIndex, Arc<PreparedScene>)>::Loading);
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
        <section class="home-heading">
            <p class="eyebrow">"A world of color. Your camera."</p>
            <h1>"See any camera's look on your photos, then get it on your camera."</h1>
        </section>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(), Load::Failed(error) => ui::failure(&error).into_any(), Load::Ready((index, scene)) => view! { <HomeChoices index scene/> }.into_any()
        }}
        <section class="home-help"><div><h2>"Built together, photo by photo"</h2><p>"Shared photos help us understand each camera's color. About a minute with photos you already have."</p></div><a class="button" href=crate::base::site_url("/contribute")>"Help build looks"</a></section>
        <section class="home-developers"><p>"Building camera firmware? Give your users a new world of looks."</p><a href=crate::base::site_url("/developers")>"For developers →"</a></section>
    }
}

#[component]
fn HomeChoices(index: SiteIndex, scene: Arc<PreparedScene>) -> impl IntoView {
    let items = cards(&index);
    let camera = RwSignal::new(index.bodies.first().map(|camera| camera.id.clone()));
    let look = RwSignal::new(items.first().map(|item| item.slug.clone()));
    let right = RwSignal::new(items.get(1).or_else(|| items.first()).cloned());
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
                let a = first.ok_or_else(|| "No looks published yet.".to_owned())?;
                let b = right.ok_or_else(|| "Choose a look.".to_owned())?;
                Ok((
                    data::preview(a.slug.as_str(), a.version.as_str()).await?,
                    data::preview(b.slug.as_str(), b.version.as_str()).await?,
                    a.name,
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
    Effect::new(move |_| {
        let name = hero_name.clone();
        wasm_bindgen_futures::spawn_local(async move {
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
                {items.into_iter().map(|item| view! {
                    <super::looks::CharacterCard item scene=scene.clone()/>
                }).collect_view()}
            </div>
        </section>
    }
}
