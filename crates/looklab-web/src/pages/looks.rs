// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use looklab_publish::{BodySummary, PreparedScene, SiteIndex};
use std::sync::Arc;

pub(crate) fn cards(index: &SiteIndex, wanted: Option<&data::WantedFile>) -> Vec<ui::LookCardItem> {
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
                evidence: wanted
                    .and_then(|wanted| data::missing_captures(wanted, index, look.slug.as_str())),
                lut: None,
            })
        })
        .collect()
}

/// Whether the public catalog has any look with a published version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogAvailability {
    Published,
    /// Every published look is an experimental estimate; none is qualified.
    ExperimentalOnly,
    NoPublishedLooks,
}

impl CatalogAvailability {
    pub(crate) fn of(index: &SiteIndex) -> Self {
        let mut published = false;
        let mut early = false;
        let mut qualified = false;
        for look in index
            .looks
            .iter()
            .filter(|look| look.visibility == looklab_publish::LookVisibility::Public)
        {
            if look.latest().is_none() {
                continue;
            }
            published = true;
            let browsable = crate::selection::browsable_version(look);
            if browsable.is_some_and(|version| {
                version.matchable
                    && version.support == looklab_publish::PublishedSupport::EarlyEstimate
            }) {
                early = true;
            }
            if crate::selection::browsable_scope(look).0
                == looklab_publish::SelectionScope::Qualified
            {
                qualified = true;
            }
        }
        if !published {
            Self::NoPublishedLooks
        } else if qualified || !early {
            Self::Published
        } else {
            Self::ExperimentalOnly
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
    bodies: Vec<BodySummary>,
    scene: Arc<PreparedScene>,
    items: Vec<ui::LookCardItem>,
    availability: CatalogAvailability,
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
    let evidence = RwSignal::new(std::collections::BTreeMap::<String, String>::new());
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            let Some(wanted) = data::wanted_optional().await else {
                return;
            };
            let Ok(index) = data::index().await else {
                return;
            };
            let mut map = std::collections::BTreeMap::new();
            for look in &index.looks {
                if let Some(detail) = data::missing_captures(&wanted, &index, look.slug.as_str()) {
                    map.insert(look.slug.as_str().to_owned(), detail);
                }
            }
            evidence.set(map);
        });
    });
    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            state.set(
                match async {
                    let index = data::index().await?;
                    let availability = CatalogAvailability::of(&index);
                    if availability == CatalogAvailability::NoPublishedLooks {
                        return Ok(Gallery::Collecting);
                    }
                    let mut items = cards(&index, None);
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
                        bodies: index.bodies,
                        scene,
                        items,
                        availability,
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
                let GalleryReady { bodies, scene, items, availability } = *ready;
                view! {
                    {(availability == CatalogAvailability::ExperimentalOnly).then(|| view! {
                        <ui::Notice
                            kind=ui::NoticeKind::Warning
                            message="These looks are experimental early estimates. Their calculations finished, but no part has been checked against independent photos yet. The numbers on these pages are not a measured rating.".to_owned()
                        />
                        <p class="field-row">
                            <a href=crate::base::site_url("/contribute/wanted")>"Help with a few more shots →"</a>
                        </p>
                    })}
                    <label for="look-filter">"Search looks"</label>
                    <input id="look-filter" type="search" placeholder="Search by name or character" on:input=move |event| filter.set(event_target_value(&event))/>
                    <ui::CameraPicker cameras=bodies.clone() selected=camera label="Filter by camera".to_owned()/>
                    <div class="look-gallery">{move || {
                        let needle = filter.get().to_lowercase();
                        let selected = camera.get();
                        items.iter()
                            .filter(|item| {
                                let matches_text = needle.is_empty()
                                    || item.name.to_lowercase().contains(&needle)
                                    || item.character.to_lowercase().contains(&needle);
                                let matches_camera = selected.as_ref().is_none_or(|id| {
                                    bodies.iter().any(|body| &body.id == id && body.looks.contains(&item.slug))
                                });
                                matches_text && matches_camera
                            })
                            .cloned()
                            .map(|mut item| {
                                item.evidence = evidence.get().get(item.slug.as_str()).cloned();
                                view! {
                                    <ui::LookCard item scene=scene.clone()/>
                                }
                            })
                            .collect_view()
                    }}</div>
                }.into_any()
            }
        }}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_model::file::{LookKind, LookVersion, Slug, WbPolicy};
    use looklab_publish::{
        ArtifactHash, ArtifactName, FamilyHeadPage, LookStatus, LookSummary, LookVisibility,
        SitePath, VersionSummary,
    };
    use looklab_schema::{FamilyTargetId, LookId, SafetyCheck, TargetVersion};

    fn published_look(report: Option<looklab_schema::ModelReportId>) -> LookSummary {
        let version = LookVersion::new("1.0.0").expect("version");
        let target = FamilyTargetId {
            family: LookId::new("provia").expect("family"),
            version: TargetVersion::new(1).expect("target version"),
        };
        LookSummary {
            slug: Slug::new("provia").expect("slug"),
            name: "Provia".to_owned(),
            kind: LookKind::NativeFamily,
            wb_policy: WbPolicy::Adaptive,
            visibility: LookVisibility::Public,
            head: Some(FamilyHeadPage {
                target,
                version: version.clone(),
                source_limited: report.is_none(),
                safety: looklab_schema::CandidateSafety {
                    numerical: SafetyCheck::Passed,
                    release_gates: SafetyCheck::Passed,
                },
                report,
                applicability: looklab_schema::Applicability::Current,
            }),
            selected: Some(looklab_publish::SelectedTargetSummary {
                version: version.clone(),
                eligibility: None,
                applicability: looklab_schema::Applicability::Current,
            }),
            versions: vec![VersionSummary {
                support: looklab_publish::PublishedSupport::Unstated,
                version,
                status: LookStatus::Published,
                matchable: true,
                card_eligible: false,
                approximated: false,
                applicability: looklab_schema::Applicability::Current,
                eligibility_status: None,
                card: None,
                manifest: SitePath::new("data/looks/provia/1.0.0/manifest.json").expect("path"),
                manifest_sha256: ArtifactHash::of(b""),
                manifest_bytes: 0,
                artifact: ArtifactName::new("look.looklab").expect("artifact"),
                sha256: ArtifactHash::of(b""),
                bytes: 0,
            }],
        }
    }

    fn index(look: LookSummary) -> SiteIndex {
        SiteIndex {
            format: looklab_publish::SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies: Vec::new(),
            manuals: Vec::new(),
            interfaces: Vec::new(),
            input_transforms: Vec::new(),
            looks: vec![look],
            scenes: Vec::new(),
            reports: Vec::new(),
            eligibility: Vec::new(),
            current: Vec::new(),
            bundles: Vec::new(),
            reference_preview: None,
        }
    }

    #[test]
    fn an_experimental_only_catalog_is_not_reported_as_ordinary() {
        let mut look = published_look(None);
        look.versions[0].support = looklab_publish::PublishedSupport::EarlyEstimate;
        assert_eq!(
            CatalogAvailability::of(&index(look)),
            CatalogAvailability::ExperimentalOnly
        );
    }

    #[test]
    fn a_source_limited_seed_with_supported_probes_is_not_called_experimental() {
        let mut look = published_look(None);
        look.versions[0].support = looklab_publish::PublishedSupport::Checked;
        assert_eq!(
            CatalogAvailability::of(&index(look)),
            CatalogAvailability::Published
        );
    }

    #[test]
    fn a_superseded_early_version_does_not_mark_the_catalog() {
        let mut look = published_look(None);
        look.versions[0].support = looklab_publish::PublishedSupport::EarlyEstimate;
        let mut current = look.versions[0].clone();
        current.version = looklab_model::file::LookVersion::new("2.0.0").expect("version");
        current.support = looklab_publish::PublishedSupport::Checked;
        look.versions.push(current);
        look.selected.as_mut().expect("selected target").version =
            looklab_model::file::LookVersion::new("2.0.0").expect("version");
        assert_eq!(
            CatalogAvailability::of(&index(look)),
            CatalogAvailability::Published
        );
    }

    #[test]
    fn a_qualified_catalog_is_published() {
        assert_eq!(
            CatalogAvailability::of(&index(published_look(Some(
                looklab_schema::ModelReportId::from_digest([5; 32])
            )))),
            CatalogAvailability::Published
        );
    }

    #[test]
    fn an_unassessed_catalog_is_not_called_experimental() {
        let mut look = published_look(None);
        look.selected = None;
        assert_eq!(
            CatalogAvailability::of(&index(look)),
            CatalogAvailability::Published
        );
    }
}
