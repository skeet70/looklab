// SPDX-License-Identifier: AGPL-3.0-or-later
use super::looks::cards;
use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_model::file::Slug;
use looklab_publish::{BodyKind, ManualSummary, PreparedScene, SceneKind, SiteIndex};
use looklab_schema::BodyId;
use std::sync::Arc;

#[derive(Clone, Debug)]
struct BodiesReady {
    index: SiteIndex,
    scene: Option<Arc<PreparedScene>>,
    preview_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CameraEntry {
    id: BodyId,
    make: String,
    model: String,
    looks: Vec<Slug>,
    manual: Option<ManualSummary>,
}

fn camera_entries(index: &SiteIndex) -> Vec<CameraEntry> {
    let mut entries = index
        .bodies
        .iter()
        .filter(|body| body.kind == BodyKind::Camera)
        .map(|body| CameraEntry {
            id: body.id.clone(),
            make: body.make.clone(),
            model: body.model.clone(),
            looks: body.looks.clone(),
            manual: None,
        })
        .collect::<Vec<_>>();
    for manual in &index.manuals {
        match entries.iter_mut().find(|entry| entry.id == manual.id) {
            Some(entry) => entry.manual = Some(manual.clone()),
            None => entries.push(CameraEntry {
                id: manual.id.clone(),
                make: manual.make.clone(),
                model: manual.model.clone(),
                looks: Vec::new(),
                manual: Some(manual.clone()),
            }),
        }
    }
    entries
}

async fn load() -> Result<BodiesReady, String> {
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

#[component]
pub fn BodiesPage() -> impl IntoView {
    let state = RwSignal::new(Load::<BodiesReady>::Loading);
    let filter = RwSignal::new(String::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match load().await {
                Ok(value) => Load::Ready(value),
                Err(error) => Load::Failed(error),
            });
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
                let mut matching = camera_entries(&index)
                    .into_iter()
                    .filter(|entry| entry.make.to_lowercase().contains(&needle) || entry.model.to_lowercase().contains(&needle))
                    .peekable();
                let no_matches = matching.peek().is_none();
                let entries = matching.map(|entry| {
                        let scene = scene.clone();
                        let published = items.iter().filter(|item| entry.looks.contains(&item.slug)).cloned().collect::<Vec<_>>();
                        let published_count = published.len();
                        let strip = published.into_iter().take(2).map(|item| match &scene {
                            Some(scene) => view! { <ui::LookCard item scene=scene.clone()/> }.into_any(),
                            None => view! { <ui::LookCard item/> }.into_any(),
                        }).collect_view();
                        let manual = entry.manual.as_ref().map(|manual| view! {
                            <p><span class="badge">"From official documentation · owner checks per control"</span></p>
                            <p>{format!(
                                "{} documented controls · {} sources · {} confirmed · {} contradicted",
                                manual.documented_controls,
                                manual.source_count,
                                manual.confirmed_controls,
                                manual.contradicted_controls,
                            )}</p>
                        });
                        view! {
                            <article class="camera-card">
                                {(published_count > 0).then(|| view! {
                                    <div class="camera-look-strip">{strip}</div>
                                })}
                                <h2>
                                    <a href=crate::base::site_url(&format!("/cameras/{}", entry.id))>
                                        {format!("{} {}", entry.make, entry.model)}
                                    </a>
                                </h2>
                                <p>{if published_count == 0 {
                                    "No measured looks published yet".to_owned()
                                } else {
                                    format!("{published_count} {} to explore", if published_count == 1 { "look" } else { "looks" })
                                }}</p>
                                {manual}
                            </article>
                        }
                    }).collect_view();
                view! {
                    {preview_error.map(|error| view! {
                        <p class="muted" role="status">"Look previews could not load. Camera details are still available."</p>
                        <ui::Disclosure summary="Preview details".to_owned()><p>{error}</p></ui::Disclosure>
                    })}
                    <div role="status">
                        {no_matches.then(|| view! {
                            <p>"No cameras match this filter. Try another make or model."</p>
                        })}
                    </div>
                    <div class="camera-gallery">{entries}</div>
                }.into_any()
            }
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_publish::{
        ArtifactHash, BodyStatus, ManualArtifact, MappingStatus, PreparationPage, SITE_FORMAT,
        SitePath,
    };

    fn body(id: &str, kind: BodyKind, looks: &[&str]) -> looklab_publish::BodySummary {
        let id = BodyId::new(id).expect("body id");
        looklab_publish::BodySummary {
            make: id.make().to_owned(),
            model: id.model().to_owned(),
            id,
            kind,
            status: BodyStatus::Active,
            mapping_status: MappingStatus::Provisional,
            preparation: PreparationPage::default(),
            epochs: 0,
            looks: looks
                .iter()
                .map(|slug| Slug::new(*slug).expect("slug"))
                .collect(),
        }
    }

    fn manual(id: &str) -> ManualSummary {
        let id = BodyId::new(id).expect("body id");
        let path = SitePath::new(format!("data/manuals/{}.json", id.as_str())).expect("site path");
        ManualSummary {
            make: id.make().to_owned(),
            model: id.model().to_owned(),
            id,
            introduced_year: None,
            source_count: 2,
            documented_controls: 7,
            confirmed_controls: 3,
            contradicted_controls: 1,
            preparation: PreparationPage::default(),
            artifact: ManualArtifact {
                path,
                sha256: ArtifactHash::of(b"manual"),
                bytes: 6,
            },
        }
    }

    fn index(bodies: Vec<looklab_publish::BodySummary>, manuals: Vec<ManualSummary>) -> SiteIndex {
        SiteIndex {
            format: SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies,
            manuals,
            looks: Vec::new(),
            scenes: Vec::new(),
            reference_preview: None,
        }
    }

    #[test]
    fn a_documented_camera_merges_with_its_measured_body() {
        let entries = camera_entries(&index(
            vec![body("fujifilm/x-t5", BodyKind::Camera, &["provia"])],
            vec![manual("fujifilm/x-t5"), manual("fujifilm/x-s10")],
        ));
        assert_eq!(entries.len(), 2);
        let t5 = entries
            .iter()
            .find(|entry| entry.id.as_str() == "fujifilm/x-t5")
            .expect("measured body");
        assert_eq!(t5.looks.len(), 1);
        assert!(t5.manual.is_some());
        let s10 = entries
            .iter()
            .find(|entry| entry.id.as_str() == "fujifilm/x-s10")
            .expect("documented camera");
        assert!(s10.looks.is_empty());
        assert!(s10.manual.is_some());
    }

    #[test]
    fn a_software_body_is_not_a_camera_card() {
        let entries = camera_entries(&index(
            vec![body("darktable/default", BodyKind::Software, &[])],
            vec![manual("fujifilm/x-s10")],
        ));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id.as_str(), "fujifilm/x-s10");
    }
}
