// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_query_map};
use looklab_publish::{LookPage, PreparedScene, SceneKind, SiteIndex};
use looklab_report::compare::{Comparison, ComparisonInput, FunctionName, LayerId, compare};
use looklab_report::svg::{
    delta_e_histogram_svg, grl_svg, region_map_svg, tint_comparison_svg, tone_comparison_svg,
};
use std::sync::Arc;

#[derive(Clone, Debug)]
struct CompareReady {
    index: SiteIndex,
    a: LookPage,
    b: LookPage,
    a_version: looklab_model::file::LookVersion,
    b_version: looklab_model::file::LookVersion,
    a_lut: Arc<Vec<u8>>,
    b_lut: Arc<Vec<u8>>,
    thumbnail: Arc<PreparedScene>,
}

#[derive(Clone, Copy)]
enum CompareField {
    LookA,
    LookB,
    VersionA,
    VersionB,
    Scene,
    View,
}

impl CompareField {
    fn key(self) -> &'static str {
        match self {
            Self::LookA => "a",
            Self::LookB => "b",
            Self::VersionA => "av",
            Self::VersionB => "bv",
            Self::Scene => "scene",
            Self::View => "view",
        }
    }
}

fn update_url(key: CompareField, value: &str) -> String {
    let url =
        web_sys::Url::new(&window().location().href().unwrap_or_default()).expect("current URL");
    url.search_params().set(key.key(), value);
    match key {
        CompareField::LookA => url.search_params().delete("av"),
        CompareField::LookB => url.search_params().delete("bv"),
        _ => {}
    }
    format!("{}{}", url.pathname(), url.search())
}

#[component]
pub fn ComparePage() -> impl IntoView {
    let query = use_query_map();
    let state = RwSignal::new(Load::<CompareReady>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let query = query.read();
        let a = query.get("a").unwrap_or_default();
        let b = query.get("b").unwrap_or_default();
        let scene = query.get("scene").unwrap_or_default();
        let av = query.get("av");
        let bv = query.get("bv");
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            let loaded = load_pair(a, b, av, bv, scene).await;
            if generation.get_untracked() == mine {
                state.set(match loaded {
                    Ok(ready) => Load::Ready(ready),
                    Err(error) => Load::Failed(error),
                });
            }
        });
    });
    view! {
        <article class="compare-page">
            <header class="compare-heading"><h1>"Compare looks"</h1><p>"One photo. Two ways to see it."</p></header>
            {move || match state.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(ready) => view! { <ComparisonView ready/> }.into_any(),
            }}
        </article>
    }
}

async fn load_pair(
    a: String,
    b: String,
    av: Option<String>,
    bv: Option<String>,
    scene: String,
) -> Result<CompareReady, String> {
    let index = data::index().await?;
    let public_looks = index
        .looks
        .iter()
        .filter(|look| look.visibility == looklab_publish::LookVisibility::Public);
    let a_slug = if a.is_empty() {
        public_looks
            .clone()
            .next()
            .map(|look| look.slug.to_string())
            .unwrap_or_default()
    } else {
        a
    };
    let b_slug = if b.is_empty() {
        public_looks
            .clone()
            .nth(1)
            .or_else(|| public_looks.clone().next())
            .map(|look| look.slug.to_string())
            .unwrap_or_default()
    } else {
        b
    };
    let a = data::look(&a_slug).await?;
    let b = data::look(&b_slug).await?;
    let a_version = av
        .map(looklab_model::file::LookVersion::new)
        .transpose()
        .map_err(|error| error.to_string())?
        .or_else(|| a.latest().map(|version| version.version().clone()))
        .ok_or("No published version for the first look")?;
    let b_version = bv
        .map(looklab_model::file::LookVersion::new)
        .transpose()
        .map_err(|error| error.to_string())?
        .or_else(|| b.latest().map(|version| version.version().clone()))
        .ok_or("No published version for the second look")?;
    let a_lut = data::preview(&a_slug, a_version.as_str()).await?;
    let b_lut = data::preview(&b_slug, b_version.as_str()).await?;
    let scene_name = if scene.is_empty() {
        index
            .scenes
            .iter()
            .find(|scene| scene.kind == SceneKind::Photo)
            .or_else(|| index.scenes.first())
            .map(|scene| scene.name.to_string())
            .ok_or("No sample photos published")?
    } else {
        scene
    };
    let thumbnail = Arc::new(data::scene_thumbnail(&scene_name).await?);
    Ok(CompareReady {
        index,
        a,
        b,
        a_version,
        b_version,
        a_lut,
        b_lut,
        thumbnail,
    })
}

#[component]
fn ComparisonView(ready: CompareReady) -> impl IntoView {
    let navigate = use_navigate();
    let change = Callback::new(move |(key, value): (CompareField, String)| {
        navigate(
            &update_url(key, &value),
            leptos_router::NavigateOptions {
                replace: true,
                scroll: false,
                ..Default::default()
            },
        );
    });
    let mode_url =
        web_sys::Url::new(&window().location().href().unwrap_or_default()).expect("current URL");
    let mode =
        ui::ViewerMode::from_query(&mode_url.search_params().get("view").unwrap_or_default());
    for (key, value) in [
        (CompareField::LookA, ready.a.slug.as_str()),
        (CompareField::LookB, ready.b.slug.as_str()),
        (CompareField::Scene, ready.thumbnail.scene.name.as_str()),
        (CompareField::View, mode.query()),
    ] {
        mode_url.search_params().set(key.key(), value);
    }
    let _ = window().history().and_then(|history| {
        history.replace_state_with_url(
            &wasm_bindgen::JsValue::NULL,
            "",
            Some(&format!("{}{}", mode_url.pathname(), mode_url.search())),
        )
    });
    let mode_change = Callback::new(move |mode: ui::ViewerMode| {
        let _ = window().history().and_then(|history| {
            history.replace_state_with_url(
                &wasm_bindgen::JsValue::NULL,
                "",
                Some(&update_url(CompareField::View, mode.query())),
            )
        });
    });
    let items = ready
        .index
        .looks
        .iter()
        .filter(|look| look.visibility == looklab_publish::LookVisibility::Public)
        .filter_map(|look| {
            let version = look.latest()?;
            let cameras = ready
                .index
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
                    looklab_model::file::LookKind::NativeFamily => {
                        ui::LookOrigin::BuiltIn { cameras }
                    }
                    looklab_model::file::LookKind::LutDefined => ui::LookOrigin::Custom {
                        author: "Community".to_owned(),
                    },
                },
                character: String::new(),
                status: version.status,
                lut: None,
            })
        })
        .collect::<Vec<_>>();
    let swap_a = ready.a.slug.to_string();
    let swap_b = ready.b.slug.to_string();
    let swap_av = ready.a_version.to_string();
    let swap_bv = ready.b_version.to_string();
    let swap_navigate = use_navigate();
    let scene = RwSignal::new(Arc::clone(&ready.thumbnail));
    let scene_error = RwSignal::new(None::<String>);
    let scene_strip_ready = RwSignal::new(false);
    let comparison = RwSignal::new(Load::<Arc<Comparison>>::Loading);
    let measurements = RwSignal::new(None::<Arc<Comparison>>);
    let scene_name = ready.thumbnail.scene.name.clone();
    let a_slug = ready.a.slug.clone();
    let b_slug = ready.b.slug.clone();
    let a_version = ready.a_version.clone();
    let b_version = ready.b_version.clone();
    let a_name = ready.a.name.clone();
    let b_name = ready.b.name.clone();
    let summary_a = a_name.clone();
    let summary_b = b_name.clone();
    Effect::new(move |_| {
        let scene_name = scene_name.clone();
        wasm_bindgen_futures::spawn_local(async move {
            ui::paint_browser().await;
            let _ = scene_strip_ready.try_set(true);
            match data::scene(scene_name.as_str()).await {
                Ok(full) => {
                    let _ = scene.try_set(Arc::new(full));
                }
                Err(error) => {
                    let _ = scene_error.try_set(Some(error));
                }
            }
        });
    });
    Effect::new(move |_| {
        let a_slug = a_slug.clone();
        let b_slug = b_slug.clone();
        let a_version = a_version.clone();
        let b_version = b_version.clone();
        let a_name = a_name.clone();
        let b_name = b_name.clone();
        wasm_bindgen_futures::spawn_local(async move {
            ui::paint_browser().await;
            if comparison.is_disposed() {
                return;
            }
            let documents = async {
                let a = data::version(a_slug.as_str(), a_version.as_str()).await?;
                let b = data::version(b_slug.as_str(), b_version.as_str()).await?;
                Ok::<_, String>((a.document, b.document))
            }
            .await;
            let (a_document, b_document) = match documents {
                Ok(documents) => documents,
                Err(error) => {
                    let _ = comparison.try_set(Load::Failed(error));
                    return;
                }
            };
            let probes = looklab_model::probes::v1();
            let a_look = crate::diagnostics::DocumentLook::new(&a_document, &probes);
            let b_look = crate::diagnostics::DocumentLook::new(&b_document, &probes);
            let result = compare(&ComparisonInput {
                a: &a_look,
                b: &b_look,
                probes: &probes,
                a_name: &FunctionName::new(a_name),
                b_name: &FunctionName::new(b_name),
            });
            let result = Arc::new(result);
            let _ = comparison.try_set(Load::Ready(Arc::clone(&result)));
            ui::paint_browser().await;
            let _ = measurements.try_set(Some(result));
        });
    });
    view! {
        <div class="compare-look-pickers">
            <div class="compare-look-chip">
                <ui::PreviewCanvas lut=Signal::stored(Some(Arc::clone(&ready.a_lut))) scene=Signal::stored(Arc::clone(&ready.thumbnail)) label=format!("Simulated: {}", ready.a.name) style=Signal::stored("width:4rem;height:3rem;object-fit:cover".to_owned())/>
                <ui::LookPicker items=items.clone() scene=Arc::clone(&ready.thumbnail) label=format!("A · {}", ready.a.name) on_select=Callback::new(move |item: ui::LookCardItem| change.run((CompareField::LookA, item.slug.to_string())))/>
            </div>
            <button type="button" class="compare-swap" aria-label="Swap looks" on:click=move |_| {
                let url = web_sys::Url::new(&window().location().href().unwrap_or_default()).expect("current URL");
                for (key, value) in [("a", &swap_b), ("b", &swap_a), ("av", &swap_bv), ("bv", &swap_av)] { url.search_params().set(key, value); }
                swap_navigate(&format!("{}{}", url.pathname(), url.search()), leptos_router::NavigateOptions { replace: true, scroll: false, ..Default::default() });
            }>"⇄"</button>
            <div class="compare-look-chip">
                <ui::PreviewCanvas lut=Signal::stored(Some(Arc::clone(&ready.b_lut))) scene=Signal::stored(Arc::clone(&ready.thumbnail)) label=format!("Simulated: {}", ready.b.name) style=Signal::stored("width:4rem;height:3rem;object-fit:cover".to_owned())/>
                <ui::LookPicker items scene=Arc::clone(&ready.thumbnail) label=format!("B · {}", ready.b.name) on_select=Callback::new(move |item: ui::LookCardItem| change.run((CompareField::LookB, item.slug.to_string())))/>
            </div>
        </div>
        <ui::Viewer lut_a=Signal::stored(Arc::clone(&ready.a_lut)) lut_b=Signal::stored(ready.b_lut) scene=Signal::derive(move || scene.get()) label_a=ready.a.name.clone() label_b=ready.b.name.clone() mode on_mode_change=mode_change/>
        {move || scene_error.get().map(|error| view! { <p class="error" role="alert">{format!("The full-size photo could not load. Showing the smaller preview. {error}")}</p> })}
        {move || match comparison.get() {
            Load::Ready(comparison) => view! { <ComparisonSummary comparison a_name=summary_a.clone() b_name=summary_b.clone()/> }.into_any(),
            Load::Loading => view! { <p class="muted" role="status">"Preparing comparison…"</p> }.into_any(),
            Load::Failed(error) => ui::failure(&error).into_any(),
        }}
        <div class="compare-scenes" aria-label="Sample photos">
            <Show when=move || scene_strip_ready.get()>
            {[SceneKind::Photo, SceneKind::TestChart].into_iter().map(|kind| {
                let scenes = ready.index.scenes.iter().filter(|scene| scene.kind == kind).cloned().collect::<Vec<_>>();
                view! { <section><h2>{if kind == SceneKind::Photo { "Sample photos" } else { "Test charts" }}</h2><div class="compare-scene-strip">{scenes.into_iter().map(|scene| {
                    let name = scene.name.to_string();
                    let selected = ready.thumbnail.scene.name == scene.name;
                    view! { <SceneChip scene lut=Arc::clone(&ready.a_lut) selected on_select=Callback::new(move |()| change.run((CompareField::Scene, name.clone())))/> }
                }).collect_view()}</div></section> }
            }).collect_view()}
            </Show>
        </div>
        <ui::Disclosure summary="More options".to_owned()>
            <div class="compare-versions">
                <label>
                    "First look version"
                    <select prop:value=ready.a_version.to_string()
                        on:change=move |event| change.run((CompareField::VersionA, event_target_value(&event)))>
                        {ready.a.versions.into_iter().map(|version| view! {
                            <option value=version.version().to_string()>
                                {version.version().to_string()}
                            </option>
                        }).collect_view()}
                    </select>
                </label>
                <label>
                    "Second look version"
                    <select prop:value=ready.b_version.to_string()
                        on:change=move |event| change.run((CompareField::VersionB, event_target_value(&event)))>
                        {ready.b.versions.into_iter().map(|version| view! {
                            <option value=version.version().to_string()>
                                {version.version().to_string()}
                            </option>
                        }).collect_view()}
                    </select>
                </label>
            </div>
        </ui::Disclosure>
        {move || measurements.get().map(|comparison| view! { <ComparisonMeasurements comparison/> })}
    }
}

#[component]
fn ComparisonSummary(comparison: Arc<Comparison>, a_name: String, b_name: String) -> impl IntoView {
    let has_measurements =
        comparison.delta_e.count > 0 && comparison.delta_e.count >= comparison.delta_e.excluded;
    let estimate = if has_measurements {
        ui::ClosenessEstimate::new(
            comparison.delta_e.median,
            [comparison.delta_e.median, comparison.delta_e.median],
            comparison.delta_e.p95,
            ui::EstimateConfidence::EarlyEstimate,
        )
    } else {
        None
    };
    let rows = [
        (
            LayerId::Tone,
            "Contrast & brightness",
            "The depth of shadows and the strength of highlights",
        ),
        (
            LayerId::Tint,
            "Neutral tint",
            "The colour cast in greys and whites",
        ),
        (LayerId::Chroma, "Saturation", "The intensity of colours"),
        (
            LayerId::Hue,
            "Hue",
            "The way colours shift around the spectrum",
        ),
        (
            LayerId::Lightness,
            "Colour brightness",
            "How light or dark individual colours appear",
        ),
    ]
    .into_iter()
    .map(|(layer, label, description)| {
        let contribution = comparison
            .attribution
            .iter()
            .find(|row| row.layer == layer)
            .map_or(0.0, |row| row.contribution.abs());
        let closeness =
            ui::closeness_level(ui::DeltaE::new(contribution).expect("finite layer contribution"))
                .label()
                .to_lowercase();
        let sentence = if has_measurements {
            format!("{description}: {closeness}.")
        } else {
            format!("{description}: not enough measured data yet.")
        };
        view! {
            <div class="compare-breakdown-row" class:no-measurements=!has_measurements>
                <strong>{label}</strong>
                {has_measurements.then(|| view! {
                    <meter min="0" max="10" value=contribution
                        aria-label=format!("{label}: {closeness}")>
                    </meter>
                })}
                <span>{sentence}</span>
            </div>
        }
    })
    .collect_view();
    let tone_delta = comparison
        .tone
        .points
        .iter()
        .map(|point| point.delta)
        .sum::<f64>()
        / comparison.tone.points.len().max(1) as f64;
    let tint_delta = comparison
        .tint
        .points
        .iter()
        .map(|point| point.delta_b)
        .sum::<f64>()
        / comparison.tint.points.len().max(1) as f64;
    let brightness = if tone_delta.abs() < 1.0 {
        "similar brightness"
    } else if tone_delta > 0.0 {
        "brighter tones"
    } else {
        "darker tones"
    };
    let warmth = if tint_delta.abs() < 0.5 {
        "a similar neutral tint"
    } else if tint_delta > 0.0 {
        "warmer neutrals"
    } else {
        "cooler neutrals"
    };
    let summary = if has_measurements {
        format!(
            "{} has {brightness} and {warmth} compared with {}.",
            b_name, a_name
        )
    } else {
        "Not enough measured data to rate this difference yet. Compare the simulated photos above."
            .to_owned()
    };
    view! {
        <section class="compare-summary" aria-label="Comparison summary">
            <div class="compare-summary-line"><p>{summary}</p>{estimate.map(|estimate| view! { <ui::Closeness estimate/> })}</div>
            <div class="compare-breakdown">{rows}</div>
        </section>
    }
}

#[component]
fn ComparisonMeasurements(comparison: Arc<Comparison>) -> impl IntoView {
    let has_measurements =
        comparison.delta_e.count > 0 && comparison.delta_e.count >= comparison.delta_e.excluded;
    let tone = tone_comparison_svg(&comparison.tone);
    let tint = tint_comparison_svg(&comparison.tint);
    let grl = comparison.grl.as_ref().map(grl_svg);
    let histogram = delta_e_histogram_svg(&comparison.delta_e);
    let ciede = delta_e_histogram_svg(&comparison.ciede2000);
    let measurement_rows = [&comparison.delta_e, &comparison.ciede2000].into_iter().map(|distribution| view! {
        <tr><th scope="row">{distribution.metric.label()}</th>
            <td>{format!("{:.2}", distribution.median)}</td><td>{format!("{:.2}", distribution.p95)}</td>
            <td>{format!("{:.2}", distribution.max)}</td><td>{format!("{:.2}", distribution.weighted_mean)}</td><td>{distribution.count}</td></tr>
    }).collect_view();
    let region = region_map_svg(&comparison.region_map);
    let attribution = comparison
        .attribution
        .iter()
        .map(|row| {
            view! {
                <tr>
                    <th scope="row">{row.layer.name()}</th>
                    <td>{format!("{:.3}", row.contribution)}</td>
                </tr>
            }
        })
        .collect_view();
    let measurement_summary = if has_measurements {
        format!(
            "CAM16-UCS ΔE median {:.2}, p95 {:.2}. Uncertainty bands are not available for this comparison.",
            comparison.delta_e.median, comparison.delta_e.p95
        )
    } else {
        "Too few supported measurement samples overlap. The overlays show model predictions, not a measured rating.".to_owned()
    };
    view! {
        <ui::Disclosure summary="Show measurements".to_owned()>
            <p>"Model estimates on supported colour samples. The difference image compares the displayed sRGB previews; these measurements compare the underlying looks."</p>
            <p>{measurement_summary}</p>
            <div class="compare-measurements-table" hidden=!has_measurements><table class="data-table"><caption>"Difference measurements"</caption>
                <thead><tr><th>"Metric"</th><th>"Median"</th><th>"p95"</th><th>"Max"</th><th>"Weighted mean"</th><th>"Samples"</th></tr></thead>
                <tbody>{measurement_rows}</tbody>
            </table></div>
            <ui::Plot title="Contrast and brightness overlays".to_owned() svg=tone/>
            <ui::Plot title="Neutral tint overlays".to_owned() svg=tint/>
            {grl.map(|svg| view! { <ui::Plot title="Saturation, hue and colour brightness".to_owned() svg/> })}
            {has_measurements.then(|| view! { <ui::Plot title="ΔE distribution".to_owned() svg=histogram/><ui::Plot title="CIEDE2000 distribution".to_owned() svg=ciede/> })}
            <ui::Plot title="Region map".to_owned() svg=region/>
            <table class="data-table" hidden=!has_measurements><caption>"Layer attribution"</caption><thead><tr><th>"Layer"</th><th>"Mean ΔE reduction"</th></tr></thead><tbody>{attribution}</tbody></table>
        </ui::Disclosure>
    }
}

#[component]
fn SceneChip(
    scene: looklab_publish::SceneSummary,
    lut: Arc<Vec<u8>>,
    selected: bool,
    on_select: Callback<()>,
) -> impl IntoView {
    let thumbnail = RwSignal::new(None::<Arc<PreparedScene>>);
    let name = scene.name;
    Effect::new(move |_| {
        let name = name.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(scene) = data::scene_thumbnail(name.as_str()).await {
                let _ = thumbnail.try_set(Some(Arc::new(scene)));
            }
        });
    });
    view! {
        <button class="compare-scene-chip" type="button" aria-pressed=selected.to_string() on:click=move |_| on_select.run(())>
            {move || thumbnail.get().map(|scene| view! { <ui::PreviewCanvas lut=Signal::stored(Some(Arc::clone(&lut))) scene=Signal::stored(scene) label="Simulated sample photo".to_owned() style=Signal::stored("display:block;width:100%;height:auto".to_owned())/> })}
            <span>{scene.title}</span>
        </button>
    }
}
