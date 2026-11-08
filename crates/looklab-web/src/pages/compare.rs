// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::data::{self, Load};
use crate::ui;
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_query_map};
use leptos_router::params::ParamsMap;
use looklab_model::file::{LookVersion, Slug};
use looklab_publish::{
    LookPage, LookSummary, LookVisibility, PreparedScene, SceneKind, SceneName, SiteIndex,
    VersionSummary,
};
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
    a_version: LookVersion,
    b_version: LookVersion,
    a_lut: Arc<Vec<u8>>,
    b_lut: Arc<Vec<u8>>,
    thumbnail: Arc<PreparedScene>,
    a_label: String,
    b_label: String,
}

/// What the compare page can show for the current query.
#[derive(Clone, Debug)]
enum CompareOutcome {
    Ready(Box<CompareReady>),
    Unavailable(Unavailable),
}

/// One identifier from the query: absent, parsed, or present but not valid.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum QueryValue<T> {
    #[default]
    Absent,
    Named(T),
    Invalid,
}

/// The query, with identifiers parsed into the domain types that validate them.
#[derive(Clone, Debug, Default)]
struct CompareQuery {
    a: QueryValue<Slug>,
    b: QueryValue<Slug>,
    a_version: QueryValue<LookVersion>,
    b_version: QueryValue<LookVersion>,
    scene: QueryValue<SceneName>,
}

impl CompareQuery {
    fn parse(query: &ParamsMap) -> Self {
        Self::from_values(|key| query.get(key))
    }

    fn from_values(value: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            a: identifier(value("a"), Slug::new),
            b: identifier(value("b"), Slug::new),
            a_version: identifier(value("av"), LookVersion::new),
            b_version: identifier(value("bv"), LookVersion::new),
            scene: identifier(value("scene"), SceneName::new),
        }
    }
}

fn identifier<T, E>(value: Option<String>, parse: fn(String) -> Result<T, E>) -> QueryValue<T> {
    match value {
        None => QueryValue::Absent,
        Some(value) if value.is_empty() => QueryValue::Absent,
        Some(value) => parse(value).map_or(QueryValue::Invalid, QueryValue::Named),
    }
}

/// One side of a comparison, resolved to a published version.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LookSelection {
    slug: Slug,
    version: LookVersion,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ComparePlan {
    a: LookSelection,
    b: LookSelection,
    scene: SceneName,
}

/// What the page will compare for a query, before any artifact is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
enum CompareResolution {
    Ready(ComparePlan),
    Unavailable(Unavailable),
}

/// A catalog look with the version a comparison would use.
#[derive(Clone, Copy)]
struct CatalogLook<'a> {
    summary: &'a LookSummary,
    latest: &'a VersionSummary,
}

/// Why the published catalog cannot supply a comparison.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Unavailable {
    NoLooks,
    UnknownLook(Slug),
    InvalidLook,
    UnknownVersion {
        slug: Slug,
        name: String,
        version: LookVersion,
    },
    InvalidVersion,
    NoSecondVersion {
        slug: Slug,
        name: String,
    },
    UnknownScene {
        a: LookSelection,
        b: LookSelection,
    },
    NoScenes,
}

#[derive(Clone, Debug)]
struct RecoveryAction {
    label: String,
    path: String,
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

fn updated_url(key: CompareField, value: &str) -> web_sys::Url {
    let url =
        web_sys::Url::new(&window().location().href().unwrap_or_default()).expect("current URL");
    url.search_params().set(key.key(), value);
    match key {
        CompareField::LookA => url.search_params().delete("av"),
        CompareField::LookB => url.search_params().delete("bv"),
        _ => {}
    }
    url
}

#[component]
pub fn ComparePage() -> impl IntoView {
    let query = use_query_map();
    let state = RwSignal::new(Load::<CompareOutcome>::Loading);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let query = CompareQuery::parse(&query.read());
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        state.set(Load::Loading);
        wasm_bindgen_futures::spawn_local(async move {
            let loaded = load_pair(query).await;
            if generation.get_untracked() == mine {
                state.set(match loaded {
                    Ok(outcome) => Load::Ready(outcome),
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
                Load::Ready(CompareOutcome::Ready(ready)) => view! { <ComparisonView ready=*ready/> }.into_any(),
                Load::Ready(CompareOutcome::Unavailable(unavailable)) => unavailable_view(unavailable).into_any(),
            }}
        </article>
    }
}

async fn load_pair(query: CompareQuery) -> Result<CompareOutcome, String> {
    let index = data::index().await?;
    let plan = match resolve_comparison(&index, &query) {
        CompareResolution::Ready(plan) => plan,
        CompareResolution::Unavailable(unavailable) => {
            return Ok(CompareOutcome::Unavailable(unavailable));
        }
    };
    let a = data::look(plan.a.slug.as_str()).await?;
    let b = data::look(plan.b.slug.as_str()).await?;
    let a_lut = data::preview(plan.a.slug.as_str(), plan.a.version.as_str()).await?;
    let b_lut = data::preview(plan.b.slug.as_str(), plan.b.version.as_str()).await?;
    let thumbnail = Arc::new(data::scene_thumbnail(plan.scene.as_str()).await?);
    let (a_label, b_label) = comparison_labels(&a, &plan.a, &b, &plan.b);
    Ok(CompareOutcome::Ready(Box::new(CompareReady {
        index,
        a,
        b,
        a_version: plan.a.version,
        b_version: plan.b.version,
        a_lut,
        b_lut,
        thumbnail,
        a_label,
        b_label,
    })))
}

fn comparison_labels(
    a: &LookPage,
    a_selection: &LookSelection,
    b: &LookPage,
    b_selection: &LookSelection,
) -> (String, String) {
    if a.slug == b.slug {
        (
            format!("{} {}", a.name, a_selection.version),
            format!("{} {}", b.name, b_selection.version),
        )
    } else {
        (a.name.clone(), b.name.clone())
    }
}

fn resolve_comparison(index: &SiteIndex, query: &CompareQuery) -> CompareResolution {
    match plan_comparison(index, query) {
        Ok(plan) => CompareResolution::Ready(plan),
        Err(unavailable) => CompareResolution::Unavailable(unavailable),
    }
}

fn plan_comparison(index: &SiteIndex, query: &CompareQuery) -> Result<ComparePlan, Unavailable> {
    let public = public_looks(index);
    let a_look = match &query.a {
        QueryValue::Named(slug) => catalog_look(index, slug)?,
        QueryValue::Invalid => return Err(Unavailable::InvalidLook),
        QueryValue::Absent => match &query.b {
            QueryValue::Named(slug) => match public
                .iter()
                .copied()
                .find(|look| look.summary.slug.as_str() != slug.as_str())
            {
                Some(look) => look,
                None => catalog_look(index, slug)?,
            },
            QueryValue::Invalid => return Err(Unavailable::InvalidLook),
            QueryValue::Absent => *public.first().ok_or(Unavailable::NoLooks)?,
        },
    };
    let b_look = match &query.b {
        QueryValue::Named(slug) => catalog_look(index, slug)?,
        QueryValue::Invalid => return Err(Unavailable::InvalidLook),
        QueryValue::Absent => public
            .iter()
            .copied()
            .find(|look| look.summary.slug != a_look.summary.slug)
            .unwrap_or(a_look),
    };
    let same_look = a_look.summary.slug == b_look.summary.slug;
    let a_version = match &query.a_version {
        QueryValue::Named(version) => published_version(a_look, version)?,
        QueryValue::Invalid => return Err(Unavailable::InvalidVersion),
        QueryValue::Absent => a_look.latest.version.clone(),
    };
    let b_version = match &query.b_version {
        QueryValue::Named(version) => published_version(b_look, version)?,
        QueryValue::Invalid => return Err(Unavailable::InvalidVersion),
        QueryValue::Absent if same_look => other_version(b_look, &a_version)?,
        QueryValue::Absent => b_look.latest.version.clone(),
    };
    let a = LookSelection {
        slug: a_look.summary.slug.clone(),
        version: a_version,
    };
    let b = LookSelection {
        slug: b_look.summary.slug.clone(),
        version: b_version,
    };
    let scene = plan_scene(index, &query.scene, &a, &b)?;
    Ok(ComparePlan { a, b, scene })
}

fn public_looks(index: &SiteIndex) -> Vec<CatalogLook<'_>> {
    index
        .looks
        .iter()
        .filter(|look| look.visibility == LookVisibility::Public)
        .filter_map(|look| {
            look.latest().map(|latest| CatalogLook {
                summary: look,
                latest,
            })
        })
        .collect()
}

fn catalog_look<'a>(index: &'a SiteIndex, slug: &Slug) -> Result<CatalogLook<'a>, Unavailable> {
    index
        .looks
        .iter()
        .find(|look| &look.slug == slug)
        .and_then(|look| {
            look.latest().map(|latest| CatalogLook {
                summary: look,
                latest,
            })
        })
        .ok_or_else(|| Unavailable::UnknownLook(slug.clone()))
}

fn published_version(
    look: CatalogLook<'_>,
    version: &LookVersion,
) -> Result<LookVersion, Unavailable> {
    look.summary
        .versions
        .iter()
        .any(|summary| &summary.version == version)
        .then(|| version.clone())
        .ok_or_else(|| Unavailable::UnknownVersion {
            slug: look.summary.slug.clone(),
            name: look.summary.name.clone(),
            version: version.clone(),
        })
}

fn other_version(look: CatalogLook<'_>, taken: &LookVersion) -> Result<LookVersion, Unavailable> {
    look.summary
        .versions
        .iter()
        .map(|summary| summary.version.clone())
        .filter(|version| version != taken)
        .max_by(looklab_publish::compare_versions)
        .ok_or_else(|| Unavailable::NoSecondVersion {
            slug: look.summary.slug.clone(),
            name: look.summary.name.clone(),
        })
}

fn plan_scene(
    index: &SiteIndex,
    requested: &QueryValue<SceneName>,
    a: &LookSelection,
    b: &LookSelection,
) -> Result<SceneName, Unavailable> {
    match requested {
        QueryValue::Absent => index
            .scenes
            .iter()
            .find(|scene| scene.kind == SceneKind::Photo)
            .or_else(|| index.scenes.first())
            .map(|scene| scene.name.clone())
            .ok_or(Unavailable::NoScenes),
        QueryValue::Named(name) if index.scenes.iter().any(|scene| &scene.name == name) => {
            Ok(name.clone())
        }
        QueryValue::Named(_) | QueryValue::Invalid => Err(Unavailable::UnknownScene {
            a: a.clone(),
            b: b.clone(),
        }),
    }
}

fn link(label: impl Into<String>, path: impl Into<String>) -> RecoveryAction {
    RecoveryAction {
        label: label.into(),
        path: path.into(),
    }
}

fn compare_href(a: &LookSelection, b: &LookSelection) -> String {
    format!(
        "/compare?a={}&av={}&b={}&bv={}",
        a.slug.as_str(),
        a.version.as_str(),
        b.slug.as_str(),
        b.version.as_str()
    )
}

impl Unavailable {
    fn content(&self) -> Option<(String, String, Vec<RecoveryAction>)> {
        let content = match self {
            Self::NoLooks => return None,
            Self::UnknownLook(slug) => (
                "That look isn't available".to_owned(),
                format!(
                    "No published look is named “{}”. It may have been renamed or withdrawn.",
                    slug.as_str()
                ),
                vec![
                    link("Compare the published looks", "/compare"),
                    link("Browse looks", "/looks"),
                ],
            ),
            Self::InvalidLook => (
                "That look isn't available".to_owned(),
                "The comparison asked for a look that is not a valid published look name. Choose a published look instead.".to_owned(),
                vec![
                    link("Compare the published looks", "/compare"),
                    link("Browse looks", "/looks"),
                ],
            ),
            Self::UnknownVersion {
                slug,
                name,
                version,
            } => (
                "That version isn't available".to_owned(),
                format!("{name} has no published version {version}."),
                vec![
                    link(format!("View {name}"), look_href(slug)),
                    link("Compare the latest versions", "/compare"),
                ],
            ),
            Self::InvalidVersion => (
                "That version isn't available".to_owned(),
                "The comparison asked for a version that is not a valid published version. Choose a published version instead.".to_owned(),
                vec![
                    link("Compare the latest versions", "/compare"),
                    link("Browse looks", "/looks"),
                ],
            ),
            Self::NoSecondVersion { slug, name } => (
                "There isn't a second look to compare".to_owned(),
                format!(
                    "Comparing needs two published looks, or two versions of the same look. {name} has one published version."
                ),
                vec![
                    link(format!("View {name}"), look_href(slug)),
                    link("Help build more looks", "/contribute"),
                ],
            ),
            Self::UnknownScene { a, b } => (
                "That sample photo isn't available".to_owned(),
                "Choose one of the published sample photos to compare these looks on.".to_owned(),
                vec![link("Use the default sample photo", compare_href(a, b))],
            ),
            Self::NoScenes => (
                "No sample photos are published yet".to_owned(),
                "A comparison needs a sample photo to render both looks on.".to_owned(),
                vec![
                    link("Help build looks", "/contribute"),
                    link("Browse looks", "/looks"),
                ],
            ),
        };
        Some(content)
    }
}

fn look_href(slug: &Slug) -> String {
    format!("/looks/{}", slug.as_str())
}

fn unavailable_view(unavailable: Unavailable) -> impl IntoView {
    crate::prefetch::route_rendered();
    let Some((title, message, actions)) = unavailable.content() else {
        return ui::no_published_looks().into_any();
    };
    view! {
        <ui::EmptyState title message>
            <p class="field-row">
                {actions.into_iter().map(|action| view! {
                    <a href=crate::base::site_url(&action.path)>{action.label}</a>
                }).collect_view()}
            </p>
        </ui::EmptyState>
    }
    .into_any()
}

#[component]
fn ComparisonView(ready: CompareReady) -> impl IntoView {
    let navigate = use_navigate();
    let change = Callback::new(move |(key, value): (CompareField, String)| {
        navigate(
            &crate::base::route_of(&updated_url(key, &value)),
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
        let url = updated_url(CompareField::View, mode.query());
        let _ = window().history().and_then(|history| {
            history.replace_state_with_url(
                &wasm_bindgen::JsValue::NULL,
                "",
                Some(&format!("{}{}", url.pathname(), url.search())),
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
    let a_name = ready.a_label.clone();
    let b_name = ready.b_label.clone();
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
                <ui::PreviewCanvas lut=Signal::stored(Some(Arc::clone(&ready.a_lut))) scene=Signal::stored(Arc::clone(&ready.thumbnail)) label=format!("Simulated: {}", ready.a_label) style=Signal::stored("width:4rem;height:3rem;object-fit:cover".to_owned())/>
                <ui::LookPicker items=items.clone() scene=Arc::clone(&ready.thumbnail) label=format!("A · {}", ready.a_label) on_select=Callback::new(move |item: ui::LookCardItem| change.run((CompareField::LookA, item.slug.to_string())))/>
            </div>
            <button type="button" class="compare-swap" aria-label="Swap looks" on:click=move |_| {
                let url = web_sys::Url::new(&window().location().href().unwrap_or_default()).expect("current URL");
                for (key, value) in [("a", &swap_b), ("b", &swap_a), ("av", &swap_bv), ("bv", &swap_av)] { url.search_params().set(key, value); }
                swap_navigate(&crate::base::route_of(&url), leptos_router::NavigateOptions { replace: true, scroll: false, ..Default::default() });
            }>"⇄"</button>
            <div class="compare-look-chip">
                <ui::PreviewCanvas lut=Signal::stored(Some(Arc::clone(&ready.b_lut))) scene=Signal::stored(Arc::clone(&ready.thumbnail)) label=format!("Simulated: {}", ready.b_label) style=Signal::stored("width:4rem;height:3rem;object-fit:cover".to_owned())/>
                <ui::LookPicker items scene=Arc::clone(&ready.thumbnail) label=format!("B · {}", ready.b_label) on_select=Callback::new(move |item: ui::LookCardItem| change.run((CompareField::LookB, item.slug.to_string())))/>
            </div>
        </div>
        <ui::Viewer lut_a=Signal::stored(Arc::clone(&ready.a_lut)) lut_b=Signal::stored(ready.b_lut) scene=Signal::derive(move || scene.get()) label_a=ready.a_label.clone() label_b=ready.b_label.clone() mode on_mode_change=mode_change/>
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

#[cfg(test)]
mod tests {
    use super::*;
    use looklab_publish::{
        ArtifactHash, ArtifactName, LookStatus, SITE_FORMAT, SceneArtifact, SceneSummary, SitePath,
        VersionSummary,
    };

    fn path(value: &str) -> SitePath {
        SitePath::new(value).expect("site path")
    }

    fn version(name: &str) -> VersionSummary {
        VersionSummary {
            version: LookVersion::new(name).expect("version"),
            status: LookStatus::Published,
            matchable: true,
            manifest: path(&format!("data/looks/x/{name}/manifest.json")),
            manifest_sha256: ArtifactHash::of(b"manifest"),
            manifest_bytes: 1,
            artifact: ArtifactName::new("look.bin").expect("artifact"),
            sha256: ArtifactHash::of(b"artifact"),
            bytes: 1,
        }
    }

    fn look(slug: &str, visibility: LookVisibility, versions: &[&str]) -> LookSummary {
        LookSummary {
            slug: Slug::new(slug).expect("slug"),
            name: slug.to_owned(),
            kind: looklab_model::file::LookKind::NativeFamily,
            wb_policy: looklab_model::file::WbPolicy::Adaptive,
            visibility,
            versions: versions.iter().copied().map(version).collect(),
        }
    }

    fn scene(name: &str) -> SceneSummary {
        SceneSummary {
            name: SceneName::new(name).expect("scene name"),
            kind: SceneKind::Photo,
            title: "Sample".to_owned(),
            credit: None,
            thumbnail: SceneArtifact {
                width: 1,
                height: 1,
                path: path("data/scenes/sample.png"),
                sha256: ArtifactHash::of(b"thumbnail"),
                bytes: 1,
            },
            width: 1,
            height: 1,
            path: path("data/scenes/sample.png"),
            sha256: ArtifactHash::of(b"scene"),
            bytes: 1,
        }
    }

    fn index(looks: Vec<LookSummary>, scenes: Vec<SceneSummary>) -> SiteIndex {
        SiteIndex {
            format: SITE_FORMAT.to_owned(),
            community_snapshot_version: 0,
            bodies: Vec::new(),
            looks,
            scenes,
            reference_preview: None,
        }
    }

    fn published_index(looks: Vec<LookSummary>) -> SiteIndex {
        index(looks, vec![scene("sample")])
    }

    fn query(a: Option<&str>, b: Option<&str>) -> CompareQuery {
        CompareQuery {
            a: a.map_or(QueryValue::Absent, |value| QueryValue::Named(slug(value))),
            b: b.map_or(QueryValue::Absent, |value| QueryValue::Named(slug(value))),
            ..CompareQuery::default()
        }
    }

    fn selection(slug_value: &str, version_value: &str) -> LookSelection {
        LookSelection {
            slug: Slug::new(slug_value).expect("slug"),
            version: LookVersion::new(version_value).expect("version"),
        }
    }

    fn slug(value: &str) -> Slug {
        Slug::new(value).expect("slug")
    }

    fn look_version(value: &str) -> LookVersion {
        LookVersion::new(value).expect("version")
    }

    fn parsed(values: &[(&str, &str)]) -> CompareQuery {
        CompareQuery::from_values(|key| {
            values
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn ready(index: &SiteIndex, query: &CompareQuery) -> ComparePlan {
        match resolve_comparison(index, query) {
            CompareResolution::Ready(plan) => plan,
            CompareResolution::Unavailable(unavailable) => {
                panic!("expected a comparison, got {unavailable:?}")
            }
        }
    }

    #[test]
    fn absent_or_empty_query_values_are_absent() {
        let absent = parsed(&[]);
        assert_eq!(absent.a, QueryValue::Absent);
        assert_eq!(absent.b, QueryValue::Absent);
        assert_eq!(absent.scene, QueryValue::Absent);
        let empty = parsed(&[("a", ""), ("b", ""), ("av", ""), ("bv", ""), ("scene", "")]);
        assert_eq!(empty.a, QueryValue::Absent);
        assert_eq!(empty.b, QueryValue::Absent);
        assert_eq!(empty.a_version, QueryValue::Absent);
        assert_eq!(empty.b_version, QueryValue::Absent);
        assert_eq!(empty.scene, QueryValue::Absent);
    }

    #[test]
    fn query_identifiers_parse_into_domain_types() {
        let query = parsed(&[("a", "sunlit"), ("av", "1.0.0"), ("scene", "sample")]);
        assert_eq!(query.a, QueryValue::Named(slug("sunlit")));
        assert_eq!(query.a_version, QueryValue::Named(look_version("1.0.0")));
        assert_eq!(
            query.scene,
            QueryValue::Named(SceneName::new("sample").expect("scene name"))
        );
        assert_eq!(query.b, QueryValue::Absent);
    }

    #[test]
    fn unparsable_scene_name_is_not_a_default_scene() {
        let query = parsed(&[("scene", "Not_A_Name")]);
        assert_eq!(query.scene, QueryValue::Invalid);
    }

    #[test]
    fn empty_catalog_has_nothing_to_compare() {
        assert_eq!(
            resolve_comparison(&published_index(Vec::new()), &query(None, None)),
            CompareResolution::Unavailable(Unavailable::NoLooks)
        );
    }

    #[test]
    fn default_selection_compares_the_first_two_public_looks() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0"]),
            look("cold", LookVisibility::Public, &["2.0.0", "1.0.0"]),
        ]);
        let plan = ready(&index, &query(None, None));
        assert_eq!(plan.a, selection("sunlit", "1.0.0"));
        assert_eq!(plan.b, selection("cold", "2.0.0"));
        assert_eq!(plan.scene, SceneName::new("sample").expect("scene name"));
    }

    #[test]
    fn one_public_look_compares_its_two_newest_versions() {
        let index = published_index(vec![look(
            "sunlit",
            LookVisibility::Public,
            &["1.0.0", "2.0.0"],
        )]);
        let plan = ready(&index, &query(None, None));
        assert_eq!(plan.a, selection("sunlit", "2.0.0"));
        assert_eq!(plan.b, selection("sunlit", "1.0.0"));
    }

    #[test]
    fn one_public_look_with_one_version_is_unavailable() {
        let index = published_index(vec![look("sunlit", LookVisibility::Public, &["1.0.0"])]);
        assert_eq!(
            resolve_comparison(&index, &query(None, None)),
            CompareResolution::Unavailable(Unavailable::NoSecondVersion {
                slug: slug("sunlit"),
                name: "sunlit".to_owned(),
            })
        );
    }

    #[test]
    fn explicit_versions_select_those_versions() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0", "2.0.0"]),
            look("cold", LookVisibility::Public, &["1.0.0", "1.1.0"]),
        ]);
        let plan = ready(
            &index,
            &CompareQuery {
                a: QueryValue::Named(slug("sunlit")),
                b: QueryValue::Named(slug("cold")),
                a_version: QueryValue::Named(look_version("1.0.0")),
                b_version: QueryValue::Named(look_version("1.1.0")),
                ..CompareQuery::default()
            },
        );
        assert_eq!(plan.a, selection("sunlit", "1.0.0"));
        assert_eq!(plan.b, selection("cold", "1.1.0"));
    }

    #[test]
    fn same_look_version_comparison_is_preserved() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0", "2.0.0"]),
            look("cold", LookVisibility::Public, &["1.0.0"]),
        ]);
        let plan = ready(
            &index,
            &CompareQuery {
                a: QueryValue::Named(slug("sunlit")),
                b: QueryValue::Named(slug("sunlit")),
                a_version: QueryValue::Named(look_version("1.0.0")),
                b_version: QueryValue::Named(look_version("2.0.0")),
                ..CompareQuery::default()
            },
        );
        assert_eq!(plan.a, selection("sunlit", "1.0.0"));
        assert_eq!(plan.b, selection("sunlit", "2.0.0"));
    }

    #[test]
    fn explicit_test_fixture_look_is_addressable() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0"]),
            look("synthetic-match", LookVisibility::TestFixture, &["1.0.0"]),
        ]);
        let plan = ready(
            &index,
            &CompareQuery {
                b: QueryValue::Named(slug("synthetic-match")),
                ..CompareQuery::default()
            },
        );
        assert_eq!(plan.a, selection("sunlit", "1.0.0"));
        assert_eq!(plan.b, selection("synthetic-match", "1.0.0"));
    }

    #[test]
    fn unknown_explicit_look_is_unavailable() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0"]),
            look("cold", LookVisibility::Public, &["1.0.0"]),
        ]);
        assert_eq!(
            resolve_comparison(&index, &query(Some("missing"), None)),
            CompareResolution::Unavailable(Unavailable::UnknownLook(slug("missing")))
        );
    }

    #[test]
    fn unknown_explicit_version_is_unavailable() {
        let index = published_index(vec![look(
            "sunlit",
            LookVisibility::Public,
            &["1.0.0", "2.0.0"],
        )]);
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    a: QueryValue::Named(slug("sunlit")),
                    b: QueryValue::Named(slug("sunlit")),
                    a_version: QueryValue::Named(look_version("9.0.0")),
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(Unavailable::UnknownVersion {
                slug: slug("sunlit"),
                name: "sunlit".to_owned(),
                version: look_version("9.0.0"),
            })
        );
    }

    #[test]
    fn unknown_explicit_scene_offers_the_default_scene_with_the_same_looks() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0"]),
            look("cold", LookVisibility::Public, &["1.0.0"]),
        ]);
        let unavailable = Unavailable::UnknownScene {
            a: selection("sunlit", "1.0.0"),
            b: selection("cold", "1.0.0"),
        };
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    scene: QueryValue::Named(SceneName::new("missing").expect("scene name")),
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(unavailable.clone())
        );
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    scene: QueryValue::Invalid,
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(unavailable.clone())
        );
        let (_, _, actions) = unavailable.content().expect("recovery content");
        assert!(
            actions
                .iter()
                .any(|action| action.path == "/compare?a=sunlit&av=1.0.0&b=cold&bv=1.0.0")
        );
    }

    #[test]
    fn invalid_explicit_identifiers_reach_their_recovery_state() {
        let index = published_index(vec![
            look("sunlit", LookVisibility::Public, &["1.0.0"]),
            look("cold", LookVisibility::Public, &["1.0.0"]),
        ]);
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    a: QueryValue::Invalid,
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(Unavailable::InvalidLook)
        );
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    b: QueryValue::Invalid,
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(Unavailable::InvalidLook)
        );
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    a_version: QueryValue::Invalid,
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(Unavailable::InvalidVersion)
        );
        assert_eq!(
            resolve_comparison(
                &index,
                &CompareQuery {
                    b_version: QueryValue::Invalid,
                    ..CompareQuery::default()
                },
            ),
            CompareResolution::Unavailable(Unavailable::InvalidVersion)
        );
    }

    #[test]
    fn catalog_without_sample_photos_is_unavailable() {
        let index = index(
            vec![
                look("sunlit", LookVisibility::Public, &["1.0.0"]),
                look("cold", LookVisibility::Public, &["1.0.0"]),
            ],
            Vec::new(),
        );
        assert_eq!(
            resolve_comparison(&index, &query(None, None)),
            CompareResolution::Unavailable(Unavailable::NoScenes)
        );
    }
}
