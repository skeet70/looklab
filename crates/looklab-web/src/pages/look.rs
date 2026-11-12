// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_navigate, use_params_map, use_query_map};
use looklab_publish::{LookPage, LookVersionPage, PreparedScene, SceneIndex, SiteIndex, WeakSpot};
use looklab_report::svg::{grl_svg, tint_curve_svg, tone_curve_svg};

use crate::data::{self, Load, LoadedVersion};
use crate::ui;

#[derive(Clone, Debug)]
struct VersionDetails {
    version: String,
    changelog: Vec<String>,
    weak_spots: Vec<WeakSpot>,
    notice: Option<String>,
    canonical: bool,
    applicability: looklab_schema::Applicability,
}

impl Default for VersionDetails {
    fn default() -> Self {
        Self {
            version: String::new(),
            changelog: Vec::new(),
            weak_spots: Vec::new(),
            notice: None,
            canonical: false,
            applicability: looklab_schema::Applicability::Unassessed,
        }
    }
}

impl VersionDetails {
    fn of(version: &LookVersionPage) -> Self {
        Self {
            version: version.version().to_string(),
            changelog: version.notes.changelog.clone(),
            weak_spots: version.notes.weak_spots.clone(),
            notice: version.notes.notice.clone(),
            canonical: version.canonical,
            applicability: version.applicability.clone(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LookTab {
    Overview,
    Measurements,
    History,
}

impl LookTab {
    fn id(self) -> ui::TabId {
        ui::TabId::new(match self {
            Self::Overview => "overview",
            Self::Measurements => "measurements",
            Self::History => "history",
        })
        .expect("valid tab")
    }

    fn from_id(id: &ui::TabId) -> Self {
        match id.as_str() {
            "measurements" => Self::Measurements,
            "history" => Self::History,
            _ => Self::Overview,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Measurements => "Measurements",
            Self::History => "History",
        }
    }
}

#[component]
pub fn LookDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<LookPage>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let slug = params.read().get("look").unwrap_or_default();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        if slug.is_empty() {
            return;
        }
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = data::look(&slug).await;
            if generation.try_get_untracked() != Some(mine) {
                return;
            }
            state.set(match result {
                Ok(page) => Load::Ready(page),
                Err(error) => Load::Failed(error),
            });
        });
    });

    view! {
        {move || match state.get() {
            Load::Loading => ui::loading().into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(page) => view! { <LookPageView page/> }.into_any(),
        }}
    }
}

#[component]
fn LookPageView(page: LookPage) -> impl IntoView {
    let query = use_query_map();
    let navigate = use_navigate();
    let default_version = crate::selection::browsable_page_version(&page)
        .map(|version| version.version().to_string())
        .unwrap_or_default();
    let selected = RwSignal::new(
        query
            .get_untracked()
            .get("version")
            .unwrap_or_else(|| default_version.clone()),
    );
    Effect::new(move |_| {
        selected.set(
            query
                .get()
                .get("version")
                .unwrap_or_else(|| default_version.clone()),
        );
    });
    let version_slug = page.slug.to_string();
    let version = RwSignal::new(Load::<Arc<LoadedVersion>>::Loading);
    let version_scope = RwSignal::new(looklab_publish::SelectionScope::Unassessed);
    let version_blockers = RwSignal::new(Vec::<String>::new());
    let evidence = RwSignal::new(None::<String>);
    let approximated = RwSignal::new(false);
    let evidence_slug = page.slug.to_string();
    let card_eligible = RwSignal::new(false);
    let scene_index = RwSignal::new(Load::<SceneIndex>::Loading);
    let scene_name = RwSignal::new(String::new());
    let scene = RwSignal::new(Load::<Arc<PreparedScene>>::Loading);
    let full_scene = RwSignal::new(None::<Arc<PreparedScene>>);
    let full_scene_error = RwSignal::new(None::<String>);
    let photo_strip_ready = RwSignal::new(false);
    let index = RwSignal::new(Load::<SiteIndex>::Loading);
    let version_generation = RwSignal::new(0_u32);
    let scene_generation = RwSignal::new(0_u32);

    Effect::new(move |_| {
        let slug = version_slug.clone();
        let chosen = selected.get();
        if slug.is_empty() || chosen.is_empty() {
            return;
        }
        let mine = version_generation.get_untracked() + 1;
        version_generation.set(mine);
        version.set(Load::Loading);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let loaded = data::version(&slug, &chosen).await;
            if version_generation.try_get_untracked() != Some(mine) {
                return;
            }
            match loaded {
                Ok(loaded) => {
                    if version_generation.try_get_untracked() == Some(mine)
                        && let (Ok(index), Ok(page)) =
                            (data::index().await, data::look(&slug).await)
                        && let Some(page_version) = page
                            .versions
                            .iter()
                            .find(|entry| entry.version().as_str() == chosen.as_str())
                    {
                        let (scope, blockers) =
                            crate::selection::version_scope(&page, &index, page_version);
                        version_scope.set(scope);
                        version_blockers.set(blockers);
                        card_eligible.set(page_version.card_eligible);
                        approximated.set(page_version.approximated);
                    }
                    version.set(Load::Ready(Arc::new(loaded)));
                }
                Err(error) => version.set(Load::Failed(error)),
            }
        });
    });

    Effect::new(move |_| {
        if version_scope.get() != looklab_publish::SelectionScope::Experimental
            || !crate::base::server_available()
        {
            return;
        }
        let slug = evidence_slug.clone();
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let Some(wanted) = data::wanted_optional().await else {
                return;
            };
            let Ok(index) = data::index().await else {
                return;
            };
            evidence.set(data::missing_captures(&wanted, &index, &slug));
        });
    });

    Effect::new(move |_| {
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            match data::scenes().await {
                Ok(index) => {
                    if let Some(first) = index
                        .scenes
                        .iter()
                        .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
                        .or_else(|| index.scenes.first())
                    {
                        scene_name.set(first.name.to_string());
                    } else {
                        scene.set(Load::Failed("No sample photos are available.".to_owned()));
                    }
                    scene_index.set(Load::Ready(index));
                }
                Err(error) => {
                    scene.set(Load::Failed(error.clone()));
                    scene_index.set(Load::Failed(error));
                }
            }
        });
    });

    Effect::new(move |_| {
        let name = scene_name.get();
        let mine = scene_generation.get_untracked() + 1;
        scene_generation.set(mine);
        full_scene.set(None);
        full_scene_error.set(None);
        if name.is_empty() {
            scene.set(Load::Loading);
            return;
        }
        scene.set(Load::Loading);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let loaded = data::scene_thumbnail(&name).await;
            if scene_generation.try_get_untracked() != Some(mine) {
                return;
            }
            match loaded {
                Ok(thumbnail) => scene.set(Load::Ready(Arc::new(thumbnail))),
                Err(error) => {
                    scene.set(Load::Failed(error));
                }
            }
        });
    });

    Effect::new(move |_| {
        if !version.with(|loaded| matches!(loaded, Load::Ready(_))) {
            return;
        }
        let Some(name) = scene.with(|loaded| loaded.ready().map(|photo| photo.scene.name.clone()))
        else {
            return;
        };
        let mine = scene_generation.get_untracked();
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            ui::paint_browser().await;
            photo_strip_ready.set(true);
            let loaded = data::scene(name.as_str()).await;
            if scene_generation.try_get_untracked() != Some(mine) {
                return;
            }
            match loaded {
                Ok(photo) => full_scene.set(Some(Arc::new(photo))),
                Err(error) => full_scene_error.set(Some(error)),
            }
        });
    });

    Effect::new(move |_| {
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            index.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    let details: Vec<VersionDetails> = page.versions.iter().map(VersionDetails::of).collect();
    let slug = page.slug.to_string();
    let look_name = page.name.clone();
    let details_for_version = details.clone();
    let tab = RwSignal::new(LookTab::Overview.id());
    let active_tab = Signal::derive(move || LookTab::from_id(&tab.get()));
    let measurements_opened = Memo::new(move |opened: Option<&bool>| {
        opened.copied().unwrap_or(false) || active_tab.get() == LookTab::Measurements
    });
    let tabs = [LookTab::Overview, LookTab::Measurements, LookTab::History]
        .into_iter()
        .map(|tab| ui::TabSpec {
            id: tab.id(),
            label: tab.label().to_owned(),
        })
        .collect();
    let overview_details = details.clone();
    let camera = RwSignal::new(None::<looklab_schema::BodyId>);
    let match_slug = slug.clone();
    let origin_slug = page.slug.clone();
    let character_name = page.name.clone();
    let origin_author = page
        .license
        .as_ref()
        .map(|license| license.attribution.clone());
    let origin_kind = page.kind;
    let description = page.description.clone();
    let card_slug = page.slug.to_string();
    let superseded_by = page.superseded_by.clone();
    let version_matchability = {
        let versions: Vec<(String, bool)> = page
            .versions
            .iter()
            .map(|entry| (entry.version().to_string(), entry.matchable))
            .collect();
        Signal::derive(move || {
            versions
                .iter()
                .find(|(version, _)| *version == selected.get())
                .is_none_or(|(_, matchable)| *matchable)
        })
    };

    view! {
        <article class="look">
            <header class="look-header"><h1>{page.name.clone()}</h1>
                {move || match version.get() { Load::Ready(loaded) => {
                    let character = character(&loaded, &character_name, &description);
                    let tags = character.split_once(": ").map(|(_, phrases)| phrases.trim_end_matches('.').replace(", and ", ", ").replace(" and ", ", ").split(", ").take(4).map(str::to_owned).collect::<Vec<_>>()).unwrap_or_default();
                    view! {
                        <ui::Badge status=loaded.manifest.status/>
                        <span class="muted">{data::selection_scope_label(version_scope.get())}</span>
                        {(version_scope.get() == looklab_publish::SelectionScope::Experimental).then(|| view! {
                            <crate::pages::ExperimentalEvidence detail=evidence.get()/>
                        })}
                        {version_blockers.get().into_iter().map(|blocker| view! { <p class="notice">{blocker}</p> }).collect_view()}
                        <p class="lede">{character}</p>
                        <ul class="character-tags">{tags.into_iter().map(|tag|view! { <li>{tag}</li> }).collect_view()}</ul>
                    }.into_any()
                }, _ => ().into_any() }}
                {move || match index.get() { Load::Ready(index) => {
                    let cameras = index.bodies.iter().filter(|body| body.looks.contains(&origin_slug)).map(|body| format!("{} {}",body.make,body.model)).collect::<Vec<_>>();
                    let origin = match origin_kind {
                        looklab_model::LookKind::LutDefined => format!("Custom look by {}", origin_author.as_deref().unwrap_or("its creator")),
                        looklab_model::LookKind::NativeFamily if cameras.is_empty() => ui::look_kind_label(origin_kind).to_owned(),
                        looklab_model::LookKind::NativeFamily => format!("Built into {}", cameras.join(", ")),
                    };
                    view! { <p class="muted">{origin}</p> }.into_any()
                }, _ => ().into_any() }}
                {superseded_by.map(|pin| {
                    let href = crate::base::site_url(&format!("/looks/{}", pin.slug.as_str()));
                    let label = format!(
                        "the {} look, version {}",
                        data::plain_id(pin.slug.as_str()),
                        pin.version.as_str()
                    );
                    view! { <p class="notice">"Superseded by " <a href=href>{label}</a></p> }
                })}
            </header>
            <ui::Tabs tabs selected=tab>
            <div hidden=move || active_tab.get() != LookTab::History>
                <h2>"History"</h2><p><code>{page.slug.to_string()}</code></p>
                <p>{format!("WB policy: {}",wb_label(page.wb_policy))}</p>
                {page.license.clone().map(|license| view! { <p>{format!("License: {} · {}",license.declaration,license.attribution)}</p> })}

            <section aria-labelledby="version-picker">
                <h2 id="version-picker">"Version"</h2>
                <label for="look-version">"Published version"</label>
                <select
                    id="look-version"
                    prop:value=move || selected.get()
                    on:change=move |event| {
                        let chosen = event_target_value(&event);
                        selected.set(chosen.clone());
                        if let Ok(url) = web_sys::Url::new(&window().location().href().unwrap_or_default()) {
                            url.search_params().set("version", &chosen);
                            navigate(&crate::base::route_of(&url), leptos_router::NavigateOptions { scroll: false, ..Default::default() });
                        }
                    }
                >
                    {page.versions.iter().map(|entry| {
                        let version = entry.version().to_string();
                        let status = ui::status_label(entry.manifest.status, true);
                        let label = status.map(|status| format!("{version} ({status})")).unwrap_or_else(|| version.clone());
                        view! {
                            <option value=version>
                                {label}
                            </option>
                        }
                    }).collect_view()}
                </select>
                {move || match version.get() {
                    Load::Loading => ui::loading().into_any(),
                    Load::Failed(error) => ui::failure(&error).into_any(),
                    Load::Ready(loaded) => {
                        let current = loaded.manifest.version.to_string();
                        let detail = details_for_version
                            .iter()
                            .find(|detail| detail.version == current)
                            .cloned()
                            .unwrap_or_default();
                        view! { <VersionView loaded details=detail/> }.into_any()
                    }
                }}
            </section>
            </div>
            <div hidden=move || active_tab.get() != LookTab::Overview>

            <section aria-label="Simulated look preview">
                {move || match (version.get(), scene.get()) {
                    (Load::Ready(loaded), Load::Ready(scene)) => {
                        let label = look_name.clone();
                        let displayed = Signal::derive(move || full_scene.get().unwrap_or_else(|| Arc::clone(&scene)));
                        view! { <ui::Viewer lut_a=Signal::stored(loaded.lut.clone()) scene=displayed label_a=label/> }.into_any()
                    }
                    (Load::Failed(error), _) => ui::failure(&error).into_any(),
                    (_, Load::Failed(error)) => ui::failure(&error).into_any(),
                    _ => ui::loading().into_any(),
                }}
                {move || full_scene_error.get().map(|error| view! {
                    <p role="status">"The full-size photo could not load. The smaller preview is still visible."</p>
                    <ui::Disclosure summary="Photo loading details".to_owned()><p>{error}</p></ui::Disclosure>
                })}
                <div class="photo-strip" aria-label="Sample photos">
                    <Show when=move || photo_strip_ready.get()>
                        {move || match scene_index.get() {
                            Load::Ready(index) => index.scenes.into_iter().filter(|scene| scene.kind == looklab_publish::SceneKind::Photo).map(|summary| view! { <PhotoChoice summary selected=scene_name version/> }).collect_view().into_any(), _ => ().into_any()
                        }}
                    </Show>
                </div>
                <p>
                    <a href=crate::base::site_url(&format!("/compare?a={slug}"))>"Compare with another look →"</a>
                </p>
            </section>
            {move || match version.get() {
                Load::Ready(loaded) => match &loaded.content {
                    data::VersionContent::SourcePreview(_) => ().into_any(),
                    data::VersionContent::Layered(layered) => {
                        let images = loaded.manifest.provenance.bodies.iter().map(|body| u64::from(body.images)).sum::<u64>();
                        let contributors = loaded.manifest.provenance.bodies.iter().map(|body| u64::from(body.credentials)).sum::<u64>();
                        let cameras = loaded.manifest.provenance.bodies.len();
                        let evidence = format!(
                            "Based on {images} {} from {contributors} {} on {cameras} {}.",
                            if images == 1 { "photo" } else { "photos" },
                            if contributors == 1 { "contributor" } else { "contributors" },
                            if cameras == 1 { "camera" } else { "cameras" },
                        );
                        let approximation = layered.document.file.definition.as_ref().and_then(|definition| {
                            let declared = definition.declared_base.as_ref()?;
                            let computation = definition.encoding.base()?;
                            Some(format!(
                                "Experimental: this version was built with {} as an approximation of the declared base {}. This does not mean the declared base's camera controls or firmware are known.",
                                data::plain_base_label(computation),
                                data::plain_base_label(declared),
                            ))
                        });
                        view! {
                            <h2>"How we know"</h2>
                            {approximation.map(|note| view! { <p class="notice" role="note">{note}</p> })}
                            <p>{evidence}</p>
                        }.into_any()
                    }
                }, _ => ().into_any()
            }}
            {move || {
                if !version_matchability.get() {
                    return view! {
                        <p class="muted">"This version is a preview only. There are no settings to match and no download."</p>
                    }.into_any();
                }
                match index.get() {
                    Load::Ready(index) => {
                        let slug = match_slug.clone();
                        let href = move || crate::base::site_url(&format!(
                            "/match?camera={}&look={slug}&version={}",
                            camera.get().map(|id| id.to_string()).unwrap_or_default(),
                            selected.get(),
                        ));
                        view! {
                            <h2>"Get this look on your camera"</h2>
                            <ui::CameraPicker cameras=index.bodies selected=camera/>
                            <a class="button" href=href>"Find my settings"</a>
                        }.into_any()
                    }
                    _ => ().into_any(),
                }
            }}
            {move || card_eligible.get().then(|| {
                let slug = card_slug.clone();
                view! {
                    <p>
                        <button on:click=move |_| {
                            let slug = slug.clone();
                            let version = selected.get_untracked();
                            spawn_local(async move {
                                if let Ok(bytes) = data::card(&slug, &version, data::CardUse::Download).await {
                                    let _ = crate::files::download("LOOK0001.LLK", &bytes, "application/octet-stream");
                                }
                            });
                        }>"Download the firmware card (.LLK)"</button>
                    </p>
                    <p class="muted">"This card is for the look itself, not for one camera. It does not use camera settings or a matching result. Firmware translates it, and that translation is not checked here."</p>
                }
            })}
            {move || (!card_eligible.get() && version_scope.get() == looklab_publish::SelectionScope::Experimental).then(|| view! {
                <p class="muted">{if approximated.get() {
                    "A firmware card for this version will appear here when available. This version approximates a declared base, so it is an experimental approximation, not the base itself."
                } else {
                    "A firmware card for this version will appear here when available. It is an experimental early estimate, and no region has been checked against independent photos."
                }}</p>
            })}
            {move || overview_details.iter().find(|details| details.version == selected.get()).map(|details| view! {
                {details.notice.clone().map(|notice| view! { <p class="notice">{notice}</p> })}
                {details.weak_spots.iter().map(|spot| view! {
                    <p>
                        {format!("Help us understand {} with a few more shots.", spot.region.to_lowercase())}
                        " "
                        <a href=crate::base::site_url("/contribute/wanted")>"See wanted shots →"</a>
                    </p>
                }).collect_view()}
            })}
            <super::recipes::RecipeSection look=page.slug.clone()/>
            </div>
            <div hidden=move || active_tab.get() != LookTab::Measurements>
                <Show when=move || measurements_opened.get()>
                {move || match version.get() {
                    Load::Ready(loaded) => view! { <Measurements loaded/> }.into_any(),
                    Load::Failed(error) => ui::failure(&error).into_any(),
                    Load::Loading => ui::loading().into_any(),
                }}
                <h2>"Test charts"</h2>
                {move || match (version.get(), scene_index.get()) {
                    (Load::Ready(loaded), Load::Ready(index)) => view! { <Charts loaded index/> }.into_any(),
                    (Load::Failed(error), _) | (_, Load::Failed(error)) => ui::failure(&error).into_any(),
                    _ => ui::loading().into_any(),
                }}
                </Show>
            </div>
            </ui::Tabs>
        </article>
    }
}

#[component]
fn VersionView(loaded: Arc<LoadedVersion>, details: VersionDetails) -> impl IntoView {
    let manifest = &loaded.manifest;
    let artifacts = manifest
        .artifacts
        .iter()
        .map(|artifact| {
            view! {
                <tr>
                    <th scope="row">{artifact.name.as_str().to_owned()}</th>
                    <td><code>{artifact.sha256.as_str().to_owned()}</code></td>
                    <td>{artifact.bytes}</td>
                </tr>
            }
        })
        .collect_view();
    let gate_checks = manifest.gate_summary.checks.clone();
    let provenance = manifest.provenance.clone();
    let version = manifest.version.to_string();
    let preview = match &loaded.content {
        data::VersionContent::Layered(_) => None,
        data::VersionContent::SourcePreview(preview) => Some(preview.as_ref().clone()),
    };

    view! {
        <div class="version">
            <p>
                <ui::StatusBadge status=manifest.status/>
                " "
                <span class="pass">"matches version manifest"</span>
            </p>
            {details.notice.clone().map(|notice| view! {
                <p class="notice" role="note">{notice}</p>
            })}
            {details.canonical.then(|| view! {
                <p class="pass">"Canonical family target for this look."</p>
            })}
            {(!details.applicability.is_current()).then(|| view! {
                <p class="notice" role="note">
                    {format!("This version is {}.", data::applicability_label(&details.applicability))}
                </p>
            })}
            {preview.as_ref().map(|preview| view! {
                <p class="notice" role="note">{data::source_preview_label(preview)}</p>
                <section aria-labelledby="source-lut">
                    <h3 id="source-lut">"Source LUT"</h3>
                    <dl class="facts">
                        <dt>"Declared base"</dt>
                        <dd>{preview.encoding.base().map(data::base_label).unwrap_or_else(|| "scene-referred".to_owned())}</dd>
                        <dt>"Composition"</dt>
                        <dd>{data::source_preview_composition(preview)}</dd>
                        <dt>"Output"</dt>
                        <dd>"sRGB"</dd>
                        {preview.emulates.clone().map(|emulates| view! {
                            <dt>"Reproduces"</dt><dd>{emulation_label(&emulates)}</dd>
                        })}
                        <dt>"Source file"</dt>
                        <dd>{format!("{} · {} bytes · {}", preview.source.name.as_str(), preview.source.bytes, preview.source.sha256.as_str())}</dd>
                    </dl>
                </section>
            })}
            <table class="data-table">
                <caption>{format!("Artifacts in version {version}")}</caption>
                <thead>
                    <tr>
                        <th scope="col">"Artifact"</th>
                        <th scope="col">"SHA-256"</th>
                        <th scope="col">"Bytes"</th>
                    </tr>
                </thead>
                <tbody>{artifacts}</tbody>
            </table>
            {preview.is_none().then(|| view! {
                <ui::Gates checks=gate_checks/>
                <section aria-labelledby="provenance">
                    <h3 id="provenance">"Provenance"</h3>
                    <ul>
                        {provenance.bodies.iter().map(|body| {
                            view! {
                                <li>
                                    {format!(
                                        "{} - epochs {} · {} images · {} credentials",
                                        body.body,
                                        if body.epochs.is_empty() { "all".to_owned() } else { body.epochs.iter().map(u32::to_string).collect::<Vec<_>>().join(", ") },
                                        body.images,
                                        body.credentials,
                                    )}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                    <dl class="facts">
                        <dt>"Anchor version"</dt><dd>{provenance.anchor_version.clone()}</dd>
                        <dt>"Root admissions"</dt><dd>{provenance.root_admissions.len()}</dd>
                        <dt>"Community snapshot"</dt><dd>{provenance.community_snapshot_version}</dd>
                        <dt>"Prep versions"</dt><dd>{provenance.prep_versions.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")}</dd>
                        <dt>"Created"</dt><dd>{provenance.created.clone()}</dd>
                        <dt>"Software"</dt><dd>{provenance.software.clone()}</dd>
                    </dl>
                </section>
            })}
            {if details.changelog.is_empty() {
                ().into_any()
            } else {
                view! {
                    <section aria-labelledby="changelog">
                        <h3 id="changelog">"Changelog"</h3>
                        <ul>{details.changelog.iter().map(|line| view! { <li>{line.clone()}</li> }).collect_view()}</ul>
                    </section>
                }.into_any()
            }}
            {if details.weak_spots.is_empty() {
                ().into_any()
            } else {
                view! {
                    <section aria-labelledby="weak-spots">
                        <h3 id="weak-spots">"Weak spots"</h3>
                        <ul>
                            {details.weak_spots.iter().map(|spot| {
                                view! {
                                    <li>
                                        <strong>{spot.region.clone()}</strong>
                                        " - "
                                        {spot.note.clone()}
                                    </li>
                                }
                            }).collect_view()}
                        </ul>
                    </section>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn Measurements(loaded: Arc<LoadedVersion>) -> impl IntoView {
    match &loaded.content {
        data::VersionContent::Layered(layered) => {
            let diagnostics = &layered.diagnostics;
            let tone = tone_curve_svg(&diagnostics.tone);
            let tint = tint_curve_svg(&diagnostics.tint);
            let grl = diagnostics.grl.as_ref().map(grl_svg);
            view! {
                <section aria-labelledby="diagnostics">
                    <h2 id="diagnostics">"Measurements"</h2>
                    <ui::Plot title="Tone: J′ against scene exposure".to_owned() svg=tone/>
                    <ui::Plot title="Neutral tint in a′b′".to_owned() svg=tint/>
                    {grl.map(|svg| view! { <ui::Plot title="Chroma, hue and lightness layers by hue".to_owned() svg=svg/> })}
                </section>
            }
            .into_any()
        }
        data::VersionContent::SourcePreview(preview) => view! {
            <section aria-labelledby="diagnostics">
                <h2 id="diagnostics">"Measurements"</h2>
                <p class="muted">{data::source_preview_label(preview)}</p>
            </section>
        }
        .into_any(),
    }
}

fn wb_label(policy: looklab_model::file::WbPolicy) -> &'static str {
    match policy {
        looklab_model::file::WbPolicy::Adaptive => "adaptive",
        looklab_model::file::WbPolicy::FixedIlluminant => "fixed illuminant",
    }
}

fn emulation_label(emulation: &looklab_model::file::LutEmulation) -> String {
    match emulation {
        looklab_model::file::LutEmulation::CameraRendering { maker, rendering } => {
            format!("{maker} {rendering}")
        }
        looklab_model::file::LutEmulation::FilmStock { name } => name.clone(),
    }
}

pub(crate) fn character(loaded: &LoadedVersion, name: &str, description: &str) -> String {
    match &loaded.content {
        data::VersionContent::Layered(layered) => {
            layered_character(&layered.document, name, description)
        }
        data::VersionContent::SourcePreview(preview) => data::source_preview_character(preview),
    }
}

fn layered_character(
    document: &looklab_model::LookDocument,
    name: &str,
    description: &str,
) -> String {
    use looklab_report::compare::{ComparisonInput, FunctionName, compare};
    let probes = looklab_model::probes::v1();
    let reference = looklab_model::Look::colorimetric();
    let look = crate::diagnostics::DocumentLook::new(document, &probes);
    let summary = compare(&ComparisonInput {
        a: &reference,
        b: &look,
        probes: &probes,
        a_name: &FunctionName::new("neutral color"),
        b_name: &FunctionName::new(name),
    })
    .summary;
    if summary.contains("ΔE") || summary.contains("cannot be compared") {
        return description.to_owned();
    }
    if summary.contains("no meaningful difference") {
        return format!("{name} stays close to neutral color.");
    }
    let mut depth = 0_u32;
    let plain = summary
        .chars()
        .filter(|ch| match ch {
            '(' => {
                depth += 1;
                false
            }
            ')' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect::<String>()
        .replace("  ", " ")
        .replace(" .", ".")
        .replace(" ,", ",")
        .replace("hue rotated by ", "a hue shift ");
    let mut words = plain.split_whitespace().peekable();
    let mut result = String::with_capacity(plain.len());
    while let Some(word) = words.next() {
        if matches!(word, "over" | "at") && words.peek() == Some(&"s") {
            words.next();
            words.next();
            let mut end = words.next().unwrap_or_default();
            if word == "over" {
                words.next();
                end = words.next().unwrap_or_default();
            }
            if let Some(punctuation) = end.chars().last().filter(|ch| matches!(ch, ',' | '.')) {
                result.push(punctuation);
            }
        } else if !word.ends_with('°') {
            if !result.is_empty() {
                result.push(' ');
            }
            result.push_str(word);
        }
    }
    result
}

#[component]
fn PhotoChoice(
    summary: looklab_publish::SceneSummary,
    selected: RwSignal<String>,
    version: RwSignal<Load<Arc<LoadedVersion>>>,
) -> impl IntoView {
    let scene = RwSignal::new(None::<Arc<PreparedScene>>);
    let name = summary.name.to_string();
    let load_name = name.clone();
    let pressed = name.clone();
    let title = summary.title.clone();
    Effect::new(move |_| {
        let name = load_name.clone();
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            if let Ok(photo) = data::scene_thumbnail(&name).await {
                scene.set(Some(Arc::new(photo)));
            }
        });
    });
    view! { <button type="button" aria-label=summary.title aria-pressed=move || (selected.get()==pressed).to_string() on:click=move |_| selected.set(name.clone())>
        {move || scene.get().map(|scene|view! { <ui::PreviewCanvas scene=Signal::stored(scene) lut=Signal::derive(move || version.get().ready().map(|loaded|loaded.lut.clone())) style=Signal::stored(String::new()) label=format!("Simulated: {title}")/> })}
    </button> }
}

#[component]
fn Charts(loaded: Arc<LoadedVersion>, index: SceneIndex) -> impl IntoView {
    let charts = index
        .scenes
        .into_iter()
        .filter(|scene| scene.kind == looklab_publish::SceneKind::TestChart)
        .collect::<Vec<_>>();
    let selected = RwSignal::new(
        charts
            .first()
            .map(|chart| chart.name.to_string())
            .unwrap_or_default(),
    );
    let scene = RwSignal::new(Load::<Arc<PreparedScene>>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let name = selected.get();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        scene.set(Load::Loading);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = data::scene(&name).await;
            if generation.get_untracked() == mine {
                scene.set(match result {
                    Ok(scene) => Load::Ready(Arc::new(scene)),
                    Err(error) => Load::Failed(error),
                });
            }
        });
    });
    view! { <div class="chart-picker">{charts.into_iter().map(|chart| { let name=chart.name.to_string(); view! { <button type="button" on:click=move |_|selected.set(name.clone())>{chart.title}</button> } }).collect_view()}</div>
        {move ||match scene.get() { Load::Ready(scene)=>view! { <ui::Viewer lut_a=Signal::stored(loaded.lut.clone()) scene=Signal::stored(scene) label_a="Test chart".to_owned()/> }.into_any(),Load::Failed(error)=>ui::failure(&error).into_any(),_=>ui::loading().into_any() }}
    }
}
