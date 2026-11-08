// SPDX-License-Identifier: AGPL-3.0-or-later
//! Camera recipes and pinned, locally computed results.

use crate::data::{self, Load};
use crate::webauthn::encode_base64url;
use crate::worker::MatchWorker;
use crate::{match_data, ui};
use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::{use_params_map, use_query_map};
use looklab_match::Permalink;
use looklab_schema::BodyId;
use looklab_wasm::{MatchInputs, MatchReply, decode_match_reply, encode_match_request};
use std::sync::Arc;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;

#[wasm_bindgen(inline_js = r#"
export function copyRecipe(text) { return navigator.clipboard.writeText(text); }
export function readRecipes(profile) { return localStorage.getItem('looklab-recipes-' + profile) || '[]'; }
export function writeRecipes(profile, text) { localStorage.setItem('looklab-recipes-' + profile, text); }
export function previewPaint() { return new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = copyRecipe)]
    fn copy_recipe(text: &str) -> js_sys::Promise;
    #[wasm_bindgen(catch, js_name = readRecipes)]
    fn read_recipes(profile: &str) -> Result<String, JsValue>;
    #[wasm_bindgen(catch, js_name = writeRecipes)]
    fn write_recipes(profile: &str, text: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(js_name = previewPaint)]
    fn preview_paint() -> js_sys::Promise;
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Application {
    #[default]
    Camera,
    Card,
    Software,
}

impl Application {
    fn from_query(value: Option<&str>) -> Self {
        match value {
            Some("card") => Self::Card,
            Some("software") => Self::Software,
            _ => Self::Camera,
        }
    }
}

/// What the published catalog can offer the match wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MatchAvailability {
    /// At least one public look has a matchable version.
    Matchable,
    /// Public looks are published, but none has a matchable version.
    PreviewOnly,
    /// No public look has a published version.
    NoPublishedLooks,
}

impl MatchAvailability {
    fn of(index: &looklab_publish::SiteIndex) -> Self {
        if super::looks::CatalogAvailability::of(index)
            == super::looks::CatalogAvailability::NoPublishedLooks
        {
            return Self::NoPublishedLooks;
        }
        if index.looks.iter().any(|look| {
            look.visibility == looklab_publish::LookVisibility::Public
                && look.latest().is_some_and(|version| version.matchable)
        }) {
            Self::Matchable
        } else {
            Self::PreviewOnly
        }
    }
}

#[component]
pub fn MatchPage() -> impl IntoView {
    let query = use_query_map();
    let state = RwSignal::new(Load::<looklab_publish::SiteIndex>::Loading);
    let camera = RwSignal::new(None::<BodyId>);
    let body = RwSignal::new(None::<looklab_publish::BodyPage>);
    let camera_error = RwSignal::new(None::<String>);
    let selected = RwSignal::new(None::<looklab_model::file::Slug>);
    let epoch = RwSignal::new(None::<u32>);
    let emphasis = RwSignal::new(looklab_match::Emphasis::Balanced);
    let step = RwSignal::new(ui::StepIndex(0));
    let furthest = RwSignal::new(ui::StepIndex(0));
    let application = RwSignal::new(Application::Camera);
    let scene = RwSignal::new(None::<Arc<looklab_publish::PreparedScene>>);
    let scene_error = RwSignal::new(None::<String>);
    let auto_run = RwSignal::new(false);
    let running = RwSignal::new(false);
    let reply = RwSignal::new(None::<MatchReply>);
    let error = RwSignal::new(None::<String>);
    let run = Callback::new(move |()| {
        if running.get_untracked() {
            return;
        }
        let Some(camera_id) = camera.get_untracked() else {
            return;
        };
        let Some(slug) = selected.get_untracked() else {
            return;
        };
        let emphasis_value = emphasis.get_untracked();
        let Some(epoch_value) = epoch.get_untracked() else {
            return;
        };
        if !body
            .get_untracked()
            .is_some_and(|page| page.id == camera_id)
        {
            return;
        }
        let application_value = application.get_untracked();
        running.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let index = data::index().await?;
                let version = index
                    .looks
                    .iter()
                    .find(|look| look.slug == slug)
                    .and_then(|look| look.latest())
                    .filter(|version| version.matchable)
                    .ok_or("Choose a matchable published look.")?
                    .version
                    .to_string();
                let loaded = data::match_version(slug.as_str(), &version).await?;
                let inputs = match_data::inputs_for_camera(
                    &loaded,
                    &camera_id,
                    &emphasis_value,
                    epoch_value,
                    0x5eed_1234,
                )?;
                match_worker(&inputs).await
            }
            .await;
            if camera.is_disposed() {
                return;
            }
            running.set(false);
            if camera.get_untracked().as_ref() != Some(&camera_id)
                || selected.get_untracked().as_ref() != Some(&slug)
                || epoch.get_untracked() != Some(epoch_value)
                || emphasis.get_untracked() != emphasis_value
                || application.get_untracked() != application_value
            {
                return;
            }
            match result {
                Ok(value) => reply.set(Some(value)),
                Err(message) => error.set(Some(message)),
            }
        });
    });
    Effect::new(move |_| {
        let prefill = query.get_untracked();
        application.set(Application::from_query(prefill.get("apply").as_deref()));
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            match data::index().await {
                Ok(index) => {
                    let chosen_camera = prefill.get("camera").and_then(|id| {
                        index
                            .bodies
                            .iter()
                            .find(|body| body.id.as_str() == id)
                            .map(|body| body.id.clone())
                    });
                    let chosen_look = prefill.get("look").and_then(|slug| {
                        index
                            .looks
                            .iter()
                            .find(|look| look.slug.as_str() == slug)
                            .map(|look| look.slug.clone())
                    });
                    selected.set(chosen_look.clone().or_else(|| {
                        index
                            .looks
                            .iter()
                            .find(|look| look.visibility == looklab_publish::LookVisibility::Public)
                            .map(|look| look.slug.clone())
                    }));
                    let photo = index
                        .scenes
                        .iter()
                        .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
                        .map(|scene| scene.name.clone());
                    auto_run.set(chosen_camera.is_some() && chosen_look.is_some());
                    camera.set(chosen_camera);
                    state.set(Load::Ready(index));
                    match photo {
                        Some(name) => match data::scene_thumbnail(name.as_str()).await {
                            Ok(value) => scene.set(Some(Arc::new(value))),
                            Err(error) => scene_error.set(Some(error)),
                        },
                        None => scene_error.set(Some("No sample photos are published.".to_owned())),
                    }
                }
                Err(message) => state.set(Load::Failed(message)),
            }
        });
    });
    Effect::new(move |_| {
        let chosen = camera.get();
        body.set(None);
        epoch.set(None);
        camera_error.set(None);
        let Some(chosen) = chosen else {
            return;
        };
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            match camera_page(&chosen).await {
                Ok(value) => {
                    let Some(newest) = value.epochs.last() else {
                        camera_error.set(Some(
                            "No supported firmware versions are published for this camera."
                                .to_owned(),
                        ));
                        return;
                    };
                    epoch.set(Some(newest.id));
                    body.set(Some(value));
                }
                Err(error) => camera_error.set(Some(error)),
            }
        });
    });
    let choices_ready = Signal::derive(move || {
        let selected_camera = camera.get();
        selected.get().is_some()
            && epoch.get().is_some()
            && body
                .get()
                .is_some_and(|page| Some(&page.id) == selected_camera.as_ref())
    });
    Effect::new(move |_| {
        if auto_run.get() && choices_ready.get() {
            auto_run.set(false);
            run.run(());
        }
    });
    view! {
        <div class="match-page">
        <header class="match-heading">
            <p class="eyebrow">"Tuned for your camera"</p>
            <h1>"Get the look"</h1>
            <p class="lede">"Dial it in. Take it out into the world."</p>
        </header>
        {move || if let Some(value) = reply.get() { view! {
            <button class="secondary" on:click=move |_| reply.set(None)>
                "Change choices"
            </button>
            <MatchResult reply=value application=application.get()/>
        }.into_any() } else { match state.get() {
            Load::Loading => ui::loading().into_any(), Load::Failed(error) => ui::failure(&error).into_any(),
            Load::Ready(index) => {
                crate::prefetch::route_rendered();
                let availability = MatchAvailability::of(&index);
                if availability == MatchAvailability::NoPublishedLooks {
                    return ui::no_published_looks().into_any();
                }
                if availability == MatchAvailability::PreviewOnly {
                    return view! {
                        <ui::EmptyState title="No looks are ready for matching yet".to_owned() message="Matching becomes available when a look has enough supported data. Share RAW and JPEG pairs to help build it.".to_owned()>
                            <p class="field-row">
                                <a class="button" href=crate::base::site_url("/contribute")>"Share photos you already have"</a>
                                <a href=crate::base::site_url("/about/how-it-works")>"How looks are made →"</a>
                            </p>
                        </ui::EmptyState>
                    }.into_any();
                }
                let items = index.looks.iter().filter(|look| look.visibility == looklab_publish::LookVisibility::Public).filter_map(|look| look.latest().filter(|version| version.matchable).map(|version| ui::LookCardItem {
                    slug: look.slug.clone(), version: version.version.clone(), name: look.name.clone(),
                    origin: ui::LookOrigin::BuiltIn { cameras: index.bodies.iter().filter(|body| body.looks.contains(&look.slug)).map(|body| format!("{} {}", body.make, body.model)).collect() },
                    character: String::new(), status: version.status, lut: None,
                })).collect::<Vec<_>>();
                let items = StoredValue::new(items);
                view! {
                    <fieldset class="match-wizard" aria-label="Look choices" disabled=move || running.get()>
                    <ui::Stepper
                        steps=vec![
                            ui::WizardStep { title: "Your camera".into(), summary: None },
                            ui::WizardStep { title: "The look".into(), summary: None },
                            ui::WizardStep { title: "How to apply it".into(), summary: None },
                        ]
                        current=step
                        furthest
                        can_advance=Signal::derive(move || choices_ready.get() && !running.get())
                        locked=running
                        on_finish=run
                        finish_label="Get my settings".to_owned()
                    >
                    <section class="match-step" hidden=move || step.get().0 != 0>
                        <h2>"What do you shoot?"</h2>
                        <ui::CameraPicker cameras=index.bodies selected=camera/>
                        {move || (camera.get().is_some() && body.get().is_none() && camera_error.get().is_none())
                            .then(|| view! { <p role="status">"Loading camera settings…"</p> })}
                        {move || camera_error.get().map(|error| view! {
                            <p role="alert">
                                "Your camera settings could not load. Choose the camera again or check your connection."
                            </p>
                            <ui::Disclosure summary="Camera details".to_owned()>
                                <p>{error}</p>
                            </ui::Disclosure>
                        })}
                        {move || body.get().filter(|body| body.epochs.len() > 1).map(|body| view! {
                            <label>
                                "Which firmware is on your camera?"
                                <select
                                    prop:value=move || epoch.get().map(|value| value.to_string()).unwrap_or_default()
                                    on:change=move |event| epoch.set(event_target_value(&event).parse().ok())
                                >
                                    {body.epochs.into_iter().map(|range| view! {
                                        <option value=range.id.to_string()>
                                            {format!("Firmware versions {} to {}", range.firmware_from, range.firmware_to)}
                                        </option>
                                    }).collect_view()}
                                </select>
                            </label>
                            <p class="muted">"Not sure? Pick the newest. This is an early estimate for the selected firmware range."</p>
                        })}
                    </section>
                    <section class="match-step" hidden=move || step.get().0 != 1>
                        <h2>"Which look?"</h2>
                        <p>{move || items.with_value(|items| {
                            items.iter().find(|item| Some(&item.slug) == selected.get().as_ref())
                                .map(|item| item.name.clone())
                        })}</p>
                        {move || view! {
                            <ui::LookPicker
                                items=items.get_value()
                                scene=scene.get()
                                on_select=Callback::new(move |item: ui::LookCardItem| selected.set(Some(item.slug)))
                            />
                        }}
                        {move || scene_error.get().map(|error| view! {
                            <p role="status">"Look thumbnails could not load. You can still choose a look."</p>
                            <ui::Disclosure summary="Thumbnail details".to_owned()>
                                <p>{error}</p>
                            </ui::Disclosure>
                        })}
                        <ui::Disclosure summary="What matters most?".to_owned()>
                            <label>
                                "Emphasis"
                                <select on:change=move |event| {
                                    if let Some(value) = looklab_match::Emphasis::parse(&event_target_value(&event)) {
                                        emphasis.set(value);
                                    }
                                }>
                                    <option value="balanced">"Balanced"</option>
                                    <option value="skin">"Skin tones"</option>
                                    <option value="landscape">"Landscapes"</option>
                                    <option value="product">"Products"</option>
                                </select>
                            </label>
                        </ui::Disclosure>
                    </section>
                    <section class="match-step" hidden=move || step.get().0 != 2>
                        <h2>"How will you apply it?"</h2>
                        <div class="match-methods">
                            <button
                                aria-pressed=move || (application.get() == Application::Camera).to_string()
                                on:click=move |_| application.set(Application::Camera)
                            >
                                <strong>"In-camera settings"</strong>
                                <span>"A menu-by-menu recipe for your camera."</span>
                            </button>
                            <button
                                aria-pressed=move || (application.get() == Application::Card).to_string()
                                on:click=move |_| application.set(Application::Card)
                            >
                                <strong>"Custom firmware"</strong>
                                <span>"A card file for firmware that loads looklab looks."</span>
                            </button>
                            <button
                                aria-pressed=move || (application.get() == Application::Software).to_string()
                                on:click=move |_| application.set(Application::Software)
                            >
                                <strong>"Other software"</strong>
                                <span>"A color lookup table for compatible software."</span>
                            </button>
                    </div></section>
                    </ui::Stepper>
                    </fieldset>
                }.into_any()
            }
        } }}
        {move || running.get().then(|| view! { <p role="status">"Finding your camera’s closest match…"</p> })}
        {move || error.get().map(|message| ui::failure(&message))}
        </div>
    }
}

#[component]
fn MatchResult(
    reply: MatchReply,
    #[prop(default = Application::Camera)] application: Application,
) -> impl IntoView {
    let preview =
        RwSignal::new(Load::<(Arc<Vec<u8>>, Arc<looklab_publish::PreparedScene>)>::Loading);
    let slug = reply.target_slug.to_string();
    let version = reply.target_version.to_string();
    let body_id = reply.body.clone();
    let body = RwSignal::new(None::<looklab_publish::BodyPage>);
    let heading = RwSignal::new("Your camera recipe".to_owned());
    let full_preview_error = RwSignal::new(None::<String>);
    let full_scene = RwSignal::new(None::<Arc<looklab_publish::PreparedScene>>);
    let camera_id = body_id.clone();
    let result_epoch = reply.epoch;
    leptos::task::spawn_local_scoped_with_cancellation(async move {
        if let Ok(mut camera) = camera_page(&camera_id).await {
            camera.epochs.retain(|epoch| epoch.id == result_epoch);
            body.set(Some(camera));
        }
    });
    Effect::new(move |_| {
        let slug = slug.clone();
        let version = version.clone();
        let body_id = body_id.clone();
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = async {
                let index = data::index().await?;
                let look_name = index
                    .looks
                    .iter()
                    .find(|look| look.slug.as_str() == slug)
                    .map(|look| look.name.as_str())
                    .unwrap_or("This look");
                let camera = index
                    .bodies
                    .iter()
                    .find(|camera| camera.id == body_id)
                    .map(|camera| format!("{} {}", camera.make, camera.model))
                    .unwrap_or_else(|| "camera".to_owned());
                heading.set(format!("{look_name} on your {camera}"));
                let scene = index
                    .scenes
                    .iter()
                    .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
                    .ok_or("No sample photos are published.")?;
                let target = data::preview(&slug, &version).await?;
                let thumbnail = data::scene_thumbnail(scene.name.as_str()).await?;
                preview.set(Load::Ready((target, Arc::new(thumbnail))));
                let _ = JsFuture::from(preview_paint()).await;
                match data::scene(scene.name.as_str()).await {
                    Ok(full) => full_scene.set(Some(Arc::new(full))),
                    Err(error) => full_preview_error.set(Some(error)),
                }
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = result {
                preview.set(Load::Failed(error));
            }
        });
    });
    let predicted = Arc::new(reply.realization_lut);
    let permalink = reply.permalink.clone();
    let href = crate::base::site_url(&format!(
        "/match/r/{}{}",
        encode_base64url(permalink.as_bytes()),
        match application {
            Application::Camera => "",
            Application::Card => "?apply=card",
            Application::Software => "?apply=software",
        }
    ));
    let message = RwSignal::new(None::<String>);
    let settings = reply
        .results
        .first()
        .map(|best| best.settings.clone())
        .unwrap_or_default();
    let copy_settings = settings.clone();
    let copy_card = reply.card.clone();
    let save = permalink.clone();
    let card = reply.card.clone();
    let export_slug = reply.target_slug.to_string();
    let export_version = reply.target_version.to_string();
    let excluded = excluded_note(&reply.excluded);
    let session = crate::session::use_session();
    let copy = move |_| {
        let text = recipe_text(&copy_card, body.get_untracked().as_ref(), &copy_settings);
        spawn_local(async move {
            let result = match JsFuture::from(copy_recipe(&text)).await {
                Ok(_) => "Settings copied. Paste them into your notes.".to_owned(),
                Err(_) => {
                    "Could not access the clipboard. Select the settings card above to copy it."
                        .to_owned()
                }
            };
            message.set(Some(result));
        });
    };
    let save_profile = move |_| {
        let save = save.clone();
        spawn_local(async move {
            message.set(Some(match save_recipe(session, &save).await {
                Ok(()) => "Saved to your profile on this device.".to_owned(),
                Err(error) => error,
            }));
        });
    };
    let download_card = move |_| {
        let slug = export_slug.clone();
        let version = export_version.clone();
        spawn_local(async move {
            let result = data::card(&slug, &version).await.and_then(|bytes| {
                crate::files::download("LOOK0001.LLK", &bytes, "application/octet-stream")
            });
            if let Err(error) = result {
                message.set(Some(error));
            }
        });
    };
    let download_table = move |_| {
        if let Load::Ready((lut, _)) = preview.get_untracked() {
            let result = preview_cube(&lut).and_then(|text| {
                crate::files::download("looklab.cube", text.as_bytes(), "text/plain")
            });
            if let Err(error) = result {
                message.set(Some(error));
            }
        }
    };
    view! {
        <section class="match-result">
            <h2>{move || heading.get()}</h2>
            {excluded.map(|note| view! { <p class="muted">{note}</p> })}
            <ui::Notice message="Sample data: these settings use a simulated camera model, not measured camera response. Do not treat this demonstration as a tested recipe.".to_owned()/>
            {move || match preview.get() {
                Load::Loading => ui::loading().into_any(),
                Load::Failed(error) => view! { <PreviewFailure error/> }.into_any(),
                Load::Ready((target, scene)) => view! {
                    <ui::Viewer
                        lut_a=Signal::derive(move || target.clone())
                        lut_b=Signal::derive({
                            let predicted = predicted.clone();
                            move || predicted.clone()
                        })
                        scene=Signal::derive(move || full_scene.get().unwrap_or_else(|| scene.clone()))
                        label_a="Target look".to_owned()
                        label_b="Your camera at these settings".to_owned()
                    />
                    <div class="match-preview-labels">
                        <span>"Left: target look"</span>
                        <span>"Right: your camera at these settings"</span>
                    </div>
                }.into_any(),
            }}
            {move || full_preview_error.get().map(|error| view! {
                <p role="status">"Showing the smaller preview. The full-size photo could not load."</p>
                <ui::Disclosure summary="Preview details".to_owned()>
                    <p>{error}</p>
                </ui::Disclosure>
            })}
            <Card card=card body=body settings=settings/>
            <div class="match-actions">
                <button on:click=copy>"Copy as text"</button>
                <a class="button secondary" href=href>"Share link"</a>
                <button class="secondary" on:click=save_profile>"Save to my profile"</button>
            </div>
            {move || message.get().map(|message| view! { <p role="status">{message}</p> })}
            {(application == Application::Card).then(|| view! {
                <section class="match-download">
                    <h3>"Card file"</h3>
                    <p>"Use firmware that supports looklab cards. Copy this file to your card’s look folder, then choose it in the firmware’s look menu."</p>
                    <a href=crate::base::site_url("/developers")>
                        "Compatible firmware and installation help"
                    </a>
                    <button on:click=download_card>"Download LOOK0001.LLK"</button>
                </section>
            })}
            {(application == Application::Software).then(|| view! {
                <section class="match-download">
                    <h3>"Other software"</h3>
                    <p>"For software that accepts a scene-linear color workflow. This table is not for applying directly to a finished JPEG."</p>
                    <button
                        disabled=move || !matches!(preview.get(), Load::Ready(_))
                        on:click=download_table
                    >
                        "Download color table (.cube)"
                    </button>
                    <p class="muted">"Colors outside the measured range are an estimate."</p>
                    <ui::Disclosure summary="Details".to_owned()>
                        <p>"Input: shaped scene-linear Rec.2020 using looklab’s preview shaper. Output: encoded sRGB. The table has 33 points per axis. Use tetrahedral interpolation, with no additional display transform."</p>
                        <p>"The download checks the table’s dimensions and rejects non-finite values. This is not an editing-app profile or an independently validated pipeline export."</p>
                        <a href=crate::base::site_url("/developers/pipelines")>
                            "Input conversion and compatible pipelines"
                        </a>
                    </ui::Disclosure>
                </section>
            })}
            <InlineVerification permalink/>
        </section>
    }
}

fn menu_name(key: &str) -> String {
    match key {
        "highlight" | "highlight_tone" => "Highlight tone",
        "shadow" | "shadow_tone" => "Shadow tone",
        "color" => "Color",
        "sharpness" => "Sharpness",
        "noise_reduction" => "Noise reduction",
        "wb_r" | "wb_shift_r" => "White balance shift: red",
        "wb_b" | "wb_shift_b" => "White balance shift: blue",
        "exposure_compensation" => "Exposure compensation",
        "dr" | "dynamic_range" => "Dynamic range",
        "color_chrome_effect" => "Color Chrome Effect",
        "color_chrome_fx_blue" => "Color Chrome FX Blue",
        "grain" => "Grain effect",
        "clarity" => "Clarity",
        _ => return key.replace('_', " "),
    }
    .to_owned()
}

fn recipe_text(
    card: &looklab_wasm::dto::CardDto,
    body: Option<&looklab_publish::BodyPage>,
    settings: &looklab_model::realization::Settings,
) -> String {
    let mut text = format!(
        "looklab settings\nFilm simulation: {}\n",
        sim_name(&card.sim)
    );
    for entry in &card.entries {
        text.push_str(&format!(
            "{}: {}\n",
            menu_name(entry.key.as_str()),
            menu_entry_value(entry, body, settings)
        ));
    }
    text
}

#[component]
pub fn Card(
    card: looklab_wasm::dto::CardDto,
    #[prop(optional)] body: Option<RwSignal<Option<looklab_publish::BodyPage>>>,
    #[prop(default = looklab_model::realization::Settings::new())]
    settings: looklab_model::realization::Settings,
) -> impl IntoView {
    let band = card.sigma_band.unwrap_or(0.0);
    let estimate = ui::ClosenessEstimate::new(
        card.median_delta_e,
        [
            (card.median_delta_e - band).max(0.0),
            card.median_delta_e + band,
        ],
        card.p95_delta_e.max(card.median_delta_e),
        if card.confidence == looklab_wasm::dto::ConfidenceDto::Low {
            ui::EstimateConfidence::EarlyEstimate
        } else {
            ui::EstimateConfidence::Established
        },
    );
    let entries = card.entries.clone();
    let settings = StoredValue::new(settings);
    let exposure = card
        .entries
        .iter()
        .find(|entry| entry.key.as_str() == "exposure_compensation" && entry.changed)
        .cloned();
    let mut regions = card
        .unreachable
        .iter()
        .filter(|region| {
            ui::DeltaE::new(region.remaining_delta_e)
                .map(ui::closeness_level)
                .is_some_and(|level| level >= ui::ClosenessLevel::Close)
        })
        .map(|region| region_name(&region.region))
        .collect::<Vec<_>>();
    regions.sort_unstable();
    regions.dedup();
    view! {
        <article class="recipe-card">
            <header>
                <p class="eyebrow">"Your settings"</p>
                <h3>"Take this recipe shooting"</h3>
                {estimate.map(|estimate| view! { <ui::Closeness estimate/> })}
            </header>
            <dl class="recipe-settings">
                <dt>"Film simulation"</dt>
                <dd>{sim_name(&card.sim)}</dd>
                {move || {
                    let mut entries = entries.clone();
                    let camera = body.and_then(|body| body.get());
                    if let Some(epoch) = camera.as_ref().and_then(|camera| camera.epochs.first()) {
                        entries.sort_by_key(|entry| {
                            epoch.controls.iter().position(|control| control.key == entry.key)
                                .unwrap_or(usize::MAX)
                        });
                    }
                    entries.into_iter().map(|entry| {
                        let value = settings.with_value(|settings| {
                            menu_entry_value(&entry, camera.as_ref(), settings)
                        });
                        view! {
                            <dt>{menu_name(entry.key.as_str())}</dt>
                            <dd>{value}</dd>
                        }
                    }).collect_view()
                }}
            </dl>
            {move || body.and_then(|body| body.get()).and_then(|camera| {
                exposure.as_ref().map(|entry| {
                    let value = settings.with_value(|settings| {
                        menu_entry_value(entry, Some(&camera), settings)
                    });
                    view! {
                        <p>
                            {format!("Use {value} {} on your {} to help match brightness.",
                                menu_name(entry.key.as_str()).to_lowercase(), camera.model)}
                        </p>
                    }
                })
            })}
            <h4>"What won’t match exactly"</h4>
            <p>"Lighting and scene colors can change the match. The split preview shows the differences predicted at these settings."</p>
            <p>{if regions.is_empty() {
                "Nothing noticeable is predicted to differ.".to_owned()
            } else {
                format!("Differences may remain in {}.", regions.join(", "))
            }}</p>
            <p class="muted">{match card.confidence {
                looklab_wasm::dto::ConfidenceDto::High => "The model has stronger support for these settings.",
                looklab_wasm::dto::ConfidenceDto::Medium => "This is an estimate. Check a few of your own shots.",
                looklab_wasm::dto::ConfidenceDto::Low => "Early estimate. More shared photos would improve this recipe.",
            }}</p>
            <ui::Disclosure summary="Details".to_owned()>
                <p>{format!("Predicted median ΔE {:.2}; p95 {:.2}; 90% band ±{:.2}.",
                    card.median_delta_e, card.p95_delta_e, band)}</p>
                {card.exposure_note.map(|note| view! { <p>{note}</p> })}
                <p>{card.residual_summary}</p>
                <p>{card.confidence_reason}</p>
            </ui::Disclosure>
        </article>
    }
}

#[component]
pub fn MatchResultPage() -> impl IntoView {
    let params = use_params_map();
    let query = use_query_map();
    let application =
        Memo::new(move |_| Application::from_query(query.get().get("apply").as_deref()));
    let state = RwSignal::new(Load::<MatchReply>::Loading);
    let pending_target = RwSignal::new(None::<(Arc<Vec<u8>>, Arc<looklab_publish::PreparedScene>)>);
    let generation = RwSignal::new(0_u32);
    Effect::new(move |_| {
        let raw = params.get().get("permalink").unwrap_or_default();
        let mine = generation.get_untracked().wrapping_add(1);
        generation.set(mine);
        state.set(Load::Loading);
        pending_target.set(None);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = async {
                let bytes = crate::webauthn::decode_base64url(&raw)
                    .ok_or("This share link is incomplete. Create a new recipe.")?;
                let text =
                    String::from_utf8(bytes).map_err(|_| "This share link is not readable.")?;
                let pinned = Permalink::parse(&text).map_err(|error| error.to_string())?;
                let target_slug = pinned.target_slug.clone();
                let target_version = pinned.target_version.clone();
                spawn_local(async move {
                    let photo = async {
                        let index = data::index().await?;
                        let scene = index
                            .scenes
                            .iter()
                            .find(|scene| scene.kind == looklab_publish::SceneKind::Photo)
                            .or_else(|| index.scenes.first())
                            .ok_or("No sample photos are available.")?;
                        let lut = data::preview(&target_slug, &target_version).await?;
                        let photo = data::scene_thumbnail(scene.name.as_str()).await?;
                        Ok::<_, String>((lut, Arc::new(photo)))
                    }
                    .await;
                    if generation.try_get_untracked() == Some(mine)
                        && let Ok(photo) = photo
                    {
                        let _ = pending_target.try_set(Some(photo));
                    }
                });
                let inputs = match_data::inputs_from_permalink(&pinned).await?;
                let reply = match_worker(&inputs).await?;
                if reply.permalink != pinned.canonical() {
                    return Err("These pinned versions no longer reproduce this recipe.".into());
                }
                Ok(reply)
            }
            .await;
            state.set(match result {
                Ok(reply) => Load::Ready(reply),
                Err(error) => Load::Failed(error),
            });
        });
    });
    view! {
        <div class="match-page">
            <h1>"Your camera recipe"</h1>
            {move || match state.get() {
                Load::Loading => view! {
                    <p role="status">"Calculating your camera settings…"</p>
                    {move || pending_target.get().map(|(lut, scene)| view! {
                        <ui::Viewer lut_a=Signal::stored(lut) scene=Signal::stored(scene) label_a="Target look".to_owned()/>
                        <p>"Target look. Your camera’s result will appear when the calculation finishes."</p>
                    })}
                }.into_any(),
                Load::Failed(error) => ui::failure(&error).into_any(),
                Load::Ready(reply) => view! {
                    <MatchResult reply application=application.get()/>
                }.into_any(),
            }}
        </div>
    }
}

async fn match_worker(inputs: &MatchInputs) -> Result<MatchReply, String> {
    let request = encode_match_request(&looklab_wasm::MatchRequest {
        inputs: inputs.clone(),
    });
    let worker = MatchWorker::spawn()?;
    let reply = worker
        .run_match(&request)
        .await
        .map_err(|failure| failure.0)?;
    decode_match_reply(&reply)
}

#[component]
fn InlineVerification(permalink: String) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let outcome = RwSignal::new(None::<looklab_wasm::VerifyReply>);
    let permalink = StoredValue::new(permalink);
    let receive = Callback::new(move |files: Vec<web_sys::File>| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let result = async {
                let local = crate::files::selected_files(files);
                if !local.iter().any(crate::files::LocalFile::is_jpeg) { return Err("Choose JPEGs shot at these settings, together with their original RAW files.".to_owned()); }
                let scanned = crate::files::scan_metadata(&local).await?;
                let pairing = looklab_wasm::inspect_metadata(&scanned).map_err(|error| error.to_string())?;
                if pairing.groups.is_empty() { return Err("Add the original RAW files alongside these JPEGs so their colors can be measured.".into()); }
                let (groups, _, failures) = crate::pages::contribute::run_files(&pairing, &local, crate::worker::default_worker_count()).await?;
                if groups.is_empty() { return Err(failures.first().map(|failure| failure.message.clone()).unwrap_or_else(|| "Add the original RAW files alongside these JPEGs so their colors can be measured.".into())); }
                let pinned = Permalink::parse(&permalink.get_value()).map_err(|error| error.to_string())?;
                let inputs = match_data::inputs_from_permalink(&pinned).await?;
                let request = looklab_wasm::encode_verify_request(&looklab_wasm::VerifyRequestMessage { inputs, observed_blobs: groups.iter().flat_map(|group| group.blobs.iter().cloned()).collect() });
                let worker = MatchWorker::spawn()?;
                let bytes = worker.run_verify(&request).await.map_err(|failure| failure.0)?;
                looklab_wasm::decode_verify_reply(&bytes)
            }.await;
            match result {
                Ok(reply) => outcome.set(Some(reply)),
                Err(message) => error.set(Some(message)),
            }
            busy.set(false);
        });
    });
    view! {
        <section class="match-verification">
            <ui::Disclosure summary="Check with your shots".to_owned()>
                <h3>"How close is your camera?"</h3>
                <p>"Shoot two to four photos at these settings. Drop the JPEGs and their original RAW files here to measure the match and get an improved suggestion. Your photos stay on this device."</p>
                <ui::DropZone
                    on_files=receive
                    accept=crate::files::camera_originals_accept_attribute()
                    folders=false
                    prompt="Drop your JPEGs and original RAW files here. Nothing is uploaded.".to_owned()
                    disabled=busy
                />
        {move || busy.get().then(|| view! { <p role="status">"Checking your photos on this device…"</p> })}
        {move || error.get().map(|message| view! { <p role="alert">{message}</p> })}
        {move || outcome.get().map(|reply| {
            let measured = ui::DeltaE::new(reply.first_measured.median).map(ui::closeness_level).map(|level| level.label()).unwrap_or("Not enough measurements");
            view! {
                <h3>"Your measured match"</h3>
                <p>{format!("{measured}, based on your photos. The improved suggestion is below.")}</p>
                <ui::Disclosure summary="Measurement details".to_owned()>
                    <p>{format!(
                        "Measured median ΔE {:.2}, p95 {:.2}, from {} color samples. The original prediction was {:.2} ±{:.2}. These photos may not cover every color or lighting condition.",
                        reply.first_measured.median,
                        reply.first_measured.p95,
                        reply.first_measured.count,
                        reply.first.median_delta_e,
                        reply.first.sigma_band,
                    )}</p>
                </ui::Disclosure>
                <Card card=reply.card settings=reply.second.settings/>
            }
        })}
            </ui::Disclosure>
        </section>
    }
}

async fn save_recipe(session: crate::session::SessionContext, text: &str) -> Result<(), String> {
    let pinned = Permalink::parse(text).map_err(|error| error.to_string())?;
    session
        .require_profile()
        .await
        .ok_or("Sign in to save this recipe.")?;
    let profile = crate::idb::get(crate::idb::StorageKey::CREDENTIAL)
        .await?
        .ok_or("Sign in again to save this recipe on this device.")?;
    let stored =
        read_recipes(&profile).map_err(|_| "Your browser could not read saved recipes.")?;
    let mut recipes: Vec<String> =
        serde_json::from_str(&stored).map_err(|_| "Saved recipes could not be read.")?;
    let canonical = pinned.canonical();
    if !recipes.contains(&canonical) {
        recipes.push(canonical);
    }
    let text = serde_json::to_string(&recipes).map_err(|error| error.to_string())?;
    write_recipes(&profile, &text).map_err(|_| "Your browser could not save this recipe.".into())
}

#[component]
pub fn SavedRecipes() -> impl IntoView {
    let recipes = RwSignal::new(Vec::<Permalink>::new());
    let session = crate::session::use_session();
    Effect::new(move |_| {
        let _ = session.state.get();
        recipes.set(Vec::new());
        spawn_local(async move {
            if let Ok(Some(profile)) = crate::idb::get(crate::idb::StorageKey::CREDENTIAL).await
                && let Ok(text) = read_recipes(&profile)
                && let Ok(saved) = serde_json::from_str::<Vec<String>>(&text)
            {
                recipes.set(
                    saved
                        .iter()
                        .filter_map(|text| Permalink::parse(text).ok())
                        .collect(),
                );
            }
        });
    });
    view! {
        <section>
            <h2>"Saved recipes"</h2>
            <p class="muted">"Saved for your profile in this browser. Keep a share link to open a recipe on another device."</p>
            {move || if recipes.get().is_empty() {
                view! {
                    <p>"No saved recipes yet. Get a look for your camera, then choose Save to my profile."</p>
                    <a href=crate::base::site_url("/match")>"Get the look"</a>
                }.into_any()
            } else {
                view! {
                    <ul>
                        {recipes.get().into_iter().map(|recipe| view! {
                            <li>
                                <a href=crate::base::site_url(&format!("/match/r/{}",
                                    encode_base64url(recipe.canonical().as_bytes())))>
                                    {format!("{} on {}", recipe.target_slug, recipe.body)}
                                </a>
                            </li>
                        }).collect_view()}
                    </ul>
                }.into_any()
            }}
        </section>
    }
}

async fn camera_page(id: &BodyId) -> Result<looklab_publish::BodyPage, String> {
    let (make, model) = id
        .as_str()
        .split_once('/')
        .ok_or("This camera address is incomplete.")?;
    data::body(make, model).await
}

fn preview_cube(bytes: &[u8]) -> Result<String, String> {
    if bytes.len() != looklab_publish::LUT_BYTES {
        return Err("This color table has the wrong dimensions.".into());
    }
    let nodes = bytes.as_chunks::<12>().0;
    let mut data = Vec::with_capacity(nodes.len());
    for b in 0..33 {
        for g in 0..33 {
            for r in 0..33 {
                let node = &nodes[(r * 33 + g) * 33 + b];
                let channel = |offset| {
                    f32::from_le_bytes([
                        node[offset],
                        node[offset + 1],
                        node[offset + 2],
                        node[offset + 3],
                    ])
                };
                data.push([channel(0), channel(4), channel(8)]);
            }
        }
    }
    if data.iter().flatten().any(|value| !value.is_finite()) {
        return Err("This color table contains invalid values.".into());
    }
    let lut = looklab_export::Lut3d {
        size: 33,
        data,
        shaper: None,
    };
    Ok(looklab_export::write_cube(
        &lut,
        "looklab shaped scene Rec2020 to encoded sRGB",
        [0.0, 1.0],
    ))
}

fn excluded_note(excluded: &[looklab_publish::ExcludedSim]) -> Option<String> {
    if excluded.is_empty() {
        return None;
    }
    let items: Vec<String> = excluded
        .iter()
        .map(|entry| {
            let reason = match entry.reason {
                looklab_publish::SimExclusion::NoPublishedLook => "no published look",
                looklab_publish::SimExclusion::NoResponse => "no published response",
            };
            format!("{} ({reason})", sim_name(&entry.sim))
        })
        .collect();
    Some(format!("Not considered: {}.", items.join(", ")))
}

fn sim_name(sim: &looklab_schema::SimKey) -> String {
    match sim.as_str() {
        "classic_chrome" => "Classic Chrome".into(),
        "provia" => "PROVIA / Standard".into(),
        "velvia" => "Velvia / Vivid".into(),
        "astia" => "ASTIA / Soft".into(),
        key => key.replace(['_', '-'], " "),
    }
}

fn menu_value(value: &str) -> String {
    match value {
        "dr100" => "DR100".into(),
        "dr200" => "DR200".into(),
        "dr400" => "DR400".into(),
        value => value.replace('_', " "),
    }
}

fn menu_entry_value(
    entry: &looklab_wasm::dto::CardEntryDto,
    body: Option<&looklab_publish::BodyPage>,
    settings: &looklab_model::realization::Settings,
) -> String {
    let step = body
        .and_then(|body| body.epochs.first())
        .and_then(|epoch| {
            epoch
                .controls
                .iter()
                .find(|control| control.key == entry.key)
        })
        .and_then(|control| control.step.as_deref())
        .and_then(|step| looklab_schema::Rational::parse_step(step).ok());
    let physical = step.and_then(|step| match settings.get(&entry.key) {
        Some(looklab_model::realization::ControlValue::Step(value)) => step.checked_mul_i64(*value),
        _ => None,
    });
    if let Some(value) = physical {
        let sign = if value.num >= 0 { "+" } else { "" };
        let units = MenuUnit::from_formatted_value(&entry.value).suffix();
        let mut denominator = step.map(|step| step.den).unwrap_or(1);
        while denominator.is_multiple_of(2) {
            denominator /= 2;
        }
        while denominator.is_multiple_of(5) {
            denominator /= 5;
        }
        if denominator == 1 {
            format!("{sign}{}{units}", value.to_f64())
        } else {
            format!("{sign}{value}{units}")
        }
    } else {
        menu_value(&entry.value)
    }
}

fn region_name(region: &str) -> String {
    match region.to_ascii_lowercase().as_str() {
        "neutral" | "neutrals" => "greys".to_owned(),
        "magenta" => "magentas".to_owned(),
        "red" => "reds".to_owned(),
        "blue" => "blues".to_owned(),
        "green" => "greens".to_owned(),
        "yellow" => "yellows".to_owned(),
        other => other.replace('_', " "),
    }
}

#[component]
fn PreviewFailure(error: String) -> impl IntoView {
    view! {
        <p role="alert">"The preview could not load. Check your connection and reopen this recipe."</p>
        <ui::Disclosure summary="Preview details".to_owned()>
            <p>{error}</p>
        </ui::Disclosure>
    }
}

enum MenuUnit {
    Scalar,
    ExposureStops,
}

impl MenuUnit {
    fn from_formatted_value(value: &str) -> Self {
        if value.ends_with(" EV") {
            Self::ExposureStops
        } else {
            Self::Scalar
        }
    }

    fn suffix(self) -> &'static str {
        match self {
            Self::Scalar => "",
            Self::ExposureStops => " EV",
        }
    }
}
