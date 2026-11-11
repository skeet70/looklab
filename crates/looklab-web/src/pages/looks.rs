// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{PreparedScene, SiteIndex};
use std::sync::Arc;

pub(crate) fn cards(index: &SiteIndex) -> Vec<ui::LookCardItem> {
    index
        .looks
        .iter()
        .filter(|look| look.visibility == looklab_publish::LookVisibility::Public)
        .filter_map(|look| {
            let version = crate::selection::browsable_version(look)?;
            let (scope, blockers) = crate::selection::browsable_scope(look);
            let cameras = index
                .bodies
                .iter()
                .filter(|camera| camera.looks.contains(&look.slug))
                .map(|camera| format!("{} {}", camera.make, camera.model))
                .collect();
            Some(ui::LookCardItem {
                slug: look.slug.clone(),
                version: version.version.clone(),
                name: look.name.clone(),
                origin: match look.kind {
                    looklab_model::LookKind::NativeFamily => ui::LookOrigin::BuiltIn { cameras },
                    looklab_model::LookKind::LutDefined => ui::LookOrigin::Custom {
                        author: "the community".to_owned(),
                    },
                },
                character: String::new(),
                status: version.status,
                scope,
                blockers,
                lut: None,
            })
        })
        .collect()
}

/// Whether the public catalog has any look with a published version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogAvailability {
    Published,
    NoPublishedLooks,
}

impl CatalogAvailability {
    pub(crate) fn of(index: &SiteIndex) -> Self {
        if index.looks.iter().any(|look| {
            look.visibility == looklab_publish::LookVisibility::Public && !look.versions.is_empty()
        }) {
            Self::Published
        } else {
            Self::NoPublishedLooks
        }
    }
}

pub(crate) async fn photo() -> Result<Arc<PreparedScene>, String> {
    let scenes = data::scenes().await?;
    let first = scenes
        .scenes
        .iter()
        .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
        .ok_or_else(|| "No sample photos are available.".to_owned())?;

    data::scene_thumbnail(first.name.as_str())
        .await
        .map(Arc::new)
}
#[component]
pub(crate) fn CharacterCard(item: ui::LookCardItem, scene: Arc<PreparedScene>) -> impl IntoView {
    let enriched = RwSignal::new(item.clone());
    Effect::new(move |_| {
        if item.lut.is_some() {
            return;
        }
        let item = item.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(loaded) = data::version(item.slug.as_str(), item.version.as_str()).await {
                let mut item = item;
                let page = data::look(item.slug.as_str()).await.ok();
                if item.character.is_empty() {
                    item.character = super::look::character(
                        &loaded,
                        &item.name,
                        page.as_ref()
                            .map(|page| page.description.as_str())
                            .unwrap_or("Character measurements are not available yet."),
                    );
                }
                item.lut = Some(loaded.lut);
                if let Some(page) = page
                    && let Some(license) = page.license
                    && matches!(item.origin, ui::LookOrigin::Custom { .. })
                {
                    item.origin = ui::LookOrigin::Custom {
                        author: license.attribution,
                    };
                }
                enriched.set(item);
            }
        });
    });
    view! { {move || view! { <ui::LookCard item=enriched.get() scene=scene.clone()/> }} }
}

#[derive(Clone, Debug)]
struct GalleryReady {
    index: SiteIndex,
    scene: Arc<PreparedScene>,
    items: Vec<ui::LookCardItem>,
}

/// What the published catalog can show on the gallery page.
#[derive(Clone, Debug)]
enum Gallery {
    Collecting,
    Ready(Box<GalleryReady>),
}

#[component]
pub fn LooksPage() -> impl IntoView {
    let state = RwSignal::new(Load::<Gallery>::Loading);
    let filter = RwSignal::new(String::new());
    let camera = RwSignal::new(None::<looklab_schema::BodyId>);
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(
                match async {
                    let index = data::index().await?;
                    if CatalogAvailability::of(&index) == CatalogAvailability::NoPublishedLooks {
                        return Ok(Gallery::Collecting);
                    }
                    let mut items = cards(&index);
                    let scene = photo().await?;
                    for item in &mut items {
                        let loaded =
                            data::version(item.slug.as_str(), item.version.as_str()).await?;
                        let page = data::look(item.slug.as_str()).await?;
                        item.character =
                            super::look::character(&loaded, &item.name, &page.description);
                        item.lut = Some(loaded.lut);
                        if let Some(license) = page.license
                            && matches!(item.origin, ui::LookOrigin::Custom { .. })
                        {
                            item.origin = ui::LookOrigin::Custom {
                                author: license.attribution,
                            };
                        }
                    }
                    Ok(Gallery::Ready(Box::new(GalleryReady {
                        index,
                        scene,
                        items,
                    })))
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
        <h1>"Look gallery"</h1>
        <p class="lede">"Discover what makes other cameras special. Bring it to yours."</p>
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(), Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(Gallery::Collecting) => {
                crate::prefetch::route_rendered();
                ui::no_published_looks().into_any()
            },
            Load::Ready(Gallery::Ready(ready)) => {
                let GalleryReady { index, scene, items } = *ready;
                view! {
                    <label for="look-filter">"Search looks"</label>
                    <input id="look-filter" type="search" placeholder="Search by name or character" on:input=move |event| filter.set(event_target_value(&event))/>
                    <ui::CameraPicker cameras=index.bodies.clone() selected=camera label="Filter by camera".to_owned()/>
                    <div class="look-gallery">{move || {
                        let needle = filter.get().to_lowercase();
                        let selected = camera.get();
                        items.iter()
                            .filter(|item| {
                                let matches_text = needle.is_empty()
                                    || item.name.to_lowercase().contains(&needle)
                                    || item.character.to_lowercase().contains(&needle);
                                let matches_camera = selected.as_ref().is_none_or(|id| {
                                    index.bodies.iter().any(|body| &body.id == id && body.looks.contains(&item.slug))
                                });
                                matches_text && matches_camera
                            })
                            .cloned()
                            .map(|item| view! {
                                <ui::LookCard item scene=scene.clone()/>
                            })
                            .collect_view()
                    }}</div>
                }.into_any()
            }
        }}
    }
}
