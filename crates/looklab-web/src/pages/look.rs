// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::hooks::use_params_map;
use looklab_publish::{LookPage, LookVersionPage, PreparedScene, SceneIndex, SiteIndex, WeakSpot};
use looklab_report::svg::{grl_svg, tint_curve_svg, tone_curve_svg};

use crate::data::{self, Load, LoadedVersion};
use crate::preview::ScenePreview;
use crate::ui;

#[derive(Clone, Debug, Default)]
struct VersionDetails {
    version: String,
    changelog: Vec<String>,
    weak_spots: Vec<WeakSpot>,
    notice: Option<String>,
}

impl VersionDetails {
    fn of(version: &LookVersionPage) -> Self {
        Self {
            version: version.version().to_string(),
            changelog: version.notes.changelog.clone(),
            weak_spots: version.notes.weak_spots.clone(),
            notice: version.notes.notice.clone(),
        }
    }
}

#[component]
pub fn LookDetail() -> impl IntoView {
    let params = use_params_map();
    let state = RwSignal::new(Load::<LookPage>::Loading);
    Effect::new(move |_| {
        let slug = params.read().get("look").unwrap_or_default();
        if slug.is_empty() {
            return;
        }
        wasm_bindgen_futures::spawn_local(async move {
            state.set(match data::look(&slug).await {
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
    let params = use_params_map();
    let initial = page
        .latest()
        .map(|version| version.version().to_string())
        .unwrap_or_default();
    let selected = RwSignal::new(initial);
    let version = RwSignal::new(Load::<Arc<LoadedVersion>>::Loading);
    let scene_index = RwSignal::new(Load::<SceneIndex>::Loading);
    let scene_name = RwSignal::new(String::new());
    let scene = RwSignal::new(Load::<Arc<PreparedScene>>::Loading);
    let index = RwSignal::new(Load::<SiteIndex>::Loading);
    let other = RwSignal::new(String::new());
    let other_lut = RwSignal::new(Load::<Arc<Vec<u8>>>::Loading);

    Effect::new(move |_| {
        let slug = params.read().get("look").unwrap_or_default();
        let chosen = selected.get();
        if slug.is_empty() || chosen.is_empty() {
            return;
        }
        version.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            version.set(match data::version(&slug, &chosen).await {
                Ok(loaded) => Load::Ready(Arc::new(loaded)),
                Err(error) => Load::Failed(error),
            });
        });
    });

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            match data::scenes().await {
                Ok(index) => {
                    if let Some(first) = index.scenes.first() {
                        scene_name.set(first.name.to_string());
                    }
                    scene_index.set(Load::Ready(index));
                }
                Err(error) => scene_index.set(Load::Failed(error)),
            }
        });
    });

    Effect::new(move |_| {
        let name = scene_name.get();
        if name.is_empty() {
            return;
        }
        scene.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            scene.set(match data::scene(&name).await {
                Ok(scene) => Load::Ready(Arc::new(scene)),
                Err(error) => Load::Failed(error),
            });
        });
    });

    Effect::new(move |_| {
        wasm_bindgen_futures::spawn_local(async move {
            index.set(match data::index().await {
                Ok(index) => Load::Ready(index),
                Err(error) => Load::Failed(error),
            });
        });
    });

    Effect::new(move |_| {
        let slug = other.get();
        if slug.is_empty() {
            return;
        }
        let Some(index) = index.get().ready().cloned() else {
            return;
        };
        let Some(look) = index.looks.iter().find(|look| look.slug.as_str() == slug) else {
            other_lut.set(Load::Failed(format!("unknown look `{slug}`")));
            return;
        };
        let Some(version) = look
            .versions
            .last()
            .map(|version| version.version.to_string())
        else {
            other_lut.set(Load::Failed(format!("{slug} has no published version")));
            return;
        };
        other_lut.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            other_lut.set(match data::preview(&slug, &version).await {
                Ok(lut) => Load::Ready(lut),
                Err(error) => Load::Failed(error),
            });
        });
    });

    let details: Vec<VersionDetails> = page.versions.iter().map(VersionDetails::of).collect();
    let slug = page.slug.to_string();
    let current_slug = slug.clone();
    let look_name = page.name.clone();
    let details_for_version = details.clone();

    view! {
        <article class="look">
            <header>
                <h1>{look_name.clone()}</h1>
                <p class="muted">{format!("{} · {}", page.slug, kind_label(page.kind))}</p>
                <dl class="facts">
                    <dt>"WB policy"</dt><dd>{wb_label(page.wb_policy)}</dd>
                    <dt>"Versions"</dt><dd>{page.versions.len()}</dd>
                </dl>
                {if page.description.is_empty() {
                    ().into_any()
                } else {
                    view! { <p>{page.description.clone()}</p> }.into_any()
                }}
                {page.license.clone().map(|license| view! {
                    <p class="license">
                        {format!("License: {} · {}", license.declaration, license.attribution)}
                    </p>
                })}
            </header>

            <section aria-labelledby="version-picker">
                <h2 id="version-picker">"Version"</h2>
                <label for="look-version">"Published version"</label>
                <select
                    id="look-version"
                    on:change=move |event| selected.set(event_target_value(&event))
                >
                    {page.versions.iter().map(|entry| {
                        let version = entry.version().to_string();
                        let status = entry.manifest.status.as_str();
                        view! {
                            <option value=version.clone() selected=version == selected.get_untracked()>
                                {format!("{version} ({status})")}
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

            <section aria-labelledby="preview">
                <h2 id="preview">"Canonical preview"</h2>
                <p>
                    "Scene-linear D65 XYZ test scenes, rendered through this Look to sRGB in the browser. "
                    "The 33³ LUT is published with the look and checked against the version manifest before use."
                </p>
                <label for="scene-select">"Scene"</label>
                <select
                    id="scene-select"
                    on:change=move |event| scene_name.set(event_target_value(&event))
                >
                    {move || match scene_index.get() {
                        Load::Ready(index) => index.scenes.iter().map(|summary| {
                            let name = summary.name.to_string();
                            let selected = name == scene_name.get();
                            let value = name.clone();
                            view! { <option value=value selected=selected>{name}</option> }
                        }).collect_view().into_any(),
                        Load::Failed(error) => view! { <option>{error}</option> }.into_any(),
                        Load::Loading => view! { <option>"Loading scenes…"</option> }.into_any(),
                    }}
                </select>
                <label for="other-preview">"Compare with another look"</label>
                <select
                    id="other-preview"
                    on:change=move |event| other.set(event_target_value(&event))
                >
                    <option value="">"None"</option>
                    {move || match index.get() {
                        Load::Ready(index) => index.looks.iter().filter(|look| look.slug.as_str() != current_slug).map(|look| {
                            let value = look.slug.to_string();
                            view! { <option value=value.clone()>{look.name.clone()}</option> }
                        }).collect_view().into_any(),
                        _ => ().into_any(),
                    }}
                </select>
                {move || match (version.get(), scene.get()) {
                    (Load::Ready(loaded), Load::Ready(scene)) => {
                        let label = format!("Preview of scene {} through {}", scene.scene.name, look_name);
                        let caption = format!(
                            "{} · {} {} · LUT applied client-side",
                            scene.scene.name,
                            look_name,
                            loaded.manifest.version
                        );
                        let other_label = format!("Preview of scene {} through the other look", scene.scene.name);
                        view! {
                            <div class:preview-pair=move || other_lut.get().ready().is_some()>
                                <ScenePreview
                                    lut=loaded.lut.clone()
                                    scene=scene.clone()
                                    label=label
                                    caption=caption
                                />
                                {move || match other_lut.get() {
                                    Load::Ready(lut) => view! {
                                        <ScenePreview
                                            lut=lut
                                            scene=scene.clone()
                                            label=other_label.clone()
                                            caption=format!("{} · {} · LUT applied client-side", scene.scene.name, other.get())
                                        />
                                    }.into_any(),
                                    Load::Failed(error) => ui::failure(&error).into_any(),
                                    Load::Loading => ().into_any(),
                                }}
                            </div>
                        }.into_any()
                    }
                    (Load::Failed(error), _) => ui::failure(&error).into_any(),
                    (_, Load::Failed(error)) => ui::failure(&error).into_any(),
                    _ => ui::loading().into_any(),
                }}
                <p>
                    <a href=format!("/compare?a={slug}")>"Open diagnostics comparison"</a>
                </p>
            </section>
        </article>
    }
}

#[component]
fn VersionView(loaded: Arc<LoadedVersion>, details: VersionDetails) -> impl IntoView {
    let manifest = &loaded.manifest;
    let diagnostics = &loaded.diagnostics;
    let tone = tone_curve_svg(&diagnostics.tone);
    let tint = tint_curve_svg(&diagnostics.tint);
    let grl = diagnostics.grl.as_ref().map(grl_svg);
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
            <section aria-labelledby="diagnostics">
                <h3 id="diagnostics">"Diagnostics"</h3>
                <ui::Plot title="Tone: J′ against scene exposure".to_owned() svg=tone/>
                <ui::Plot title="Neutral tint in a′b′".to_owned() svg=tint/>
                {grl.map(|svg| view! { <ui::Plot title="Chroma, hue and lightness layers by hue".to_owned() svg=svg/> })}
            </section>
        </div>
    }
}

fn kind_label(kind: looklab_model::file::LookKind) -> &'static str {
    match kind {
        looklab_model::file::LookKind::NativeFamily => "native family",
        looklab_model::file::LookKind::LutDefined => "LUT-defined",
    }
}

fn wb_label(policy: looklab_model::file::WbPolicy) -> &'static str {
    match policy {
        looklab_model::file::WbPolicy::Adaptive => "adaptive",
        looklab_model::file::WbPolicy::FixedIlluminant => "fixed illuminant",
    }
}
