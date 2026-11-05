// SPDX-License-Identifier: AGPL-3.0-or-later

use std::sync::Arc;

use leptos::html::{Canvas, Div};
use leptos::prelude::*;
use looklab_model::file::{LookVersion, Slug};
use looklab_publish::{LookStatus, PreparedScene, render_preview};
use wasm_bindgen::{Clamped, JsCast, JsValue, prelude::wasm_bindgen};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

#[wasm_bindgen(inline_js = r#"
export function looklabDivider(element) {
    let position = 50;
    const set = value => {
        position = Math.max(0, Math.min(100, value));
        element.parentElement.style.setProperty('--split', `${position}%`);
        element.setAttribute('aria-valuenow', String(Math.round(position)));
    };
    const move = event => {
        const bounds = element.parentElement.getBoundingClientRect();
        if (bounds.width > 0) set(100 * (event.clientX - bounds.left) / bounds.width);
    };
    const down = event => {
        if (event.button !== 0) return;
        event.preventDefault();
        element.setPointerCapture(event.pointerId);
        move(event);
    };
    const drag = event => {
        if (element.hasPointerCapture(event.pointerId)) move(event);
    };
    const up = event => {
        if (element.hasPointerCapture(event.pointerId)) element.releasePointerCapture(event.pointerId);
    };
    const key = event => {
        const values = {ArrowLeft: position - 1, ArrowDown: position - 1, ArrowRight: position + 1, ArrowUp: position + 1, Home: 0, End: 100};
        if (Object.hasOwn(values, event.key)) {
            event.preventDefault();
            set(values[event.key]);
        }
    };
    element.addEventListener('pointerdown', down);
    element.addEventListener('pointermove', drag);
    element.addEventListener('pointerup', up);
    element.addEventListener('pointercancel', up);
    element.addEventListener('keydown', key);
    return () => {
        element.removeEventListener('pointerdown', down);
        element.removeEventListener('pointermove', drag);
        element.removeEventListener('pointerup', up);
        element.removeEventListener('pointercancel', up);
        element.removeEventListener('keydown', key);
    };
}
export function looklabVisible(element) {
    let observer;
    let resolve;
    const promise = new Promise(done => { resolve = done; });
    observer = new IntersectionObserver(entries => {
        if (entries.some(entry => entry.isIntersecting)) {
            observer.disconnect();
            resolve(true);
        }
    });
    observer.observe(element);
    return {promise, cancel: () => { observer.disconnect(); resolve(false); }};
}
export function looklabYield() { return new Promise(resolve => setTimeout(resolve, 0)); }
"#)]
extern "C" {
    #[wasm_bindgen(js_name = looklabDivider)]
    fn bind_divider(element: &web_sys::Element) -> js_sys::Function;
    #[wasm_bindgen(js_name = looklabVisible)]
    fn observe_visible(element: &web_sys::Element) -> JsValue;
    #[wasm_bindgen(js_name = looklabYield)]
    pub(crate) fn yield_browser() -> js_sys::Promise;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewerMode {
    Single,
    #[default]
    Split,
    SideBySide,
    Toggle,
    Difference,
}

impl ViewerMode {
    pub fn from_query(value: &str) -> Self {
        match value {
            "side-by-side" => Self::SideBySide,
            "toggle" => Self::Toggle,
            "difference" => Self::Difference,
            _ => Self::Split,
        }
    }

    pub fn query(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Split => "split",
            Self::SideBySide => "side-by-side",
            Self::Toggle => "toggle",
            Self::Difference => "difference",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Single => "Single",
            Self::Split => "Split",
            Self::SideBySide => "Side by side",
            Self::Toggle => "Toggle",
            Self::Difference => "Difference",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surround {
    #[default]
    MidGray119,
    Black,
    White,
}

impl Surround {
    fn color(self) -> &'static str {
        match self {
            Self::MidGray119 => "rgb(119,119,119)",
            Self::Black => "rgb(0,0,0)",
            Self::White => "rgb(255,255,255)",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DifferenceSupport {
    #[default]
    Unavailable,
}

fn paint(canvas: &HtmlCanvasElement, lut: &[u8], scene: &PreparedScene) -> Result<(), String> {
    let rgba = render_preview(lut, scene).map_err(|error| error.to_string())?;
    let context = canvas
        .get_context("2d")
        .map_err(|_| "2D canvas is unavailable".to_owned())?
        .ok_or_else(|| "2D canvas is unavailable".to_owned())?
        .dyn_into::<CanvasRenderingContext2d>()
        .map_err(|_| "2D canvas is unavailable".to_owned())?;
    let image =
        ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba), scene.width(), scene.height())
            .map_err(|_| "Could not prepare the simulated image".to_owned())?;
    canvas.set_width(scene.width());
    canvas.set_height(scene.height());
    context
        .put_image_data(&image, 0.0, 0.0)
        .map_err(|_| "Could not draw the simulated image".to_owned())
}

#[component]
pub(crate) fn PreviewCanvas(
    lut: Signal<Option<Arc<Vec<u8>>>>,
    scene: Signal<Arc<PreparedScene>>,
    label: String,
    style: Signal<String>,
) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let Some(element) = canvas.get() else {
            return;
        };
        let Some(lut) = lut.get() else {
            return;
        };
        let scene = scene.get();
        error.set(paint(&element, &lut, &scene).err());
    });
    view! {
        <canvas node_ref=canvas role="img" aria-label=label style=move || style.get()></canvas>
        {move || error.get().map(|message| view! { <p role="alert">{message}</p> })}
    }
}

struct DifferenceInputs {
    a: Arc<Vec<u8>>,
    b: Arc<Vec<u8>>,
    scene: Arc<PreparedScene>,
}

impl DifferenceInputs {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.a, &other.a)
            && Arc::ptr_eq(&self.b, &other.b)
            && Arc::ptr_eq(&self.scene, &other.scene)
    }
}

#[component]
fn DifferenceCanvas(
    lut_a: Signal<Arc<Vec<u8>>>,
    lut_b: Signal<Arc<Vec<u8>>>,
    scene: Signal<Arc<PreparedScene>>,
    visible: Signal<bool>,
) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    let status = RwSignal::new("Preparing difference image…".to_owned());
    let generation = RwSignal::new(0_u32);
    let computed = StoredValue::new(None::<DifferenceInputs>);
    Effect::new(move |_| {
        let inputs = DifferenceInputs {
            a: lut_a.get(),
            b: lut_b.get(),
            scene: scene.get(),
        };
        let shown = visible.get();
        let mine = generation.get_untracked() + 1;
        generation.set(mine);
        if !shown
            || computed.with_value(|cached| {
                cached
                    .as_ref()
                    .is_some_and(|cached| cached.matches(&inputs))
            })
        {
            return;
        }
        let Some(canvas) = canvas.get() else { return };
        status.set("Preparing difference image…".to_owned());
        wasm_bindgen_futures::spawn_local(async move {
            let start = js_sys::Date::now();
            let result =
                crate::diagnostics::difference_image(&inputs.a, &inputs.b, &inputs.scene).await;
            if generation.try_get_untracked() != Some(mine) {
                return;
            }
            match result {
                Ok(image) => {
                    let drawn = (|| {
                        let context = canvas
                            .get_context("2d")
                            .ok()??
                            .dyn_into::<CanvasRenderingContext2d>()
                            .ok()?;
                        let data = ImageData::new_with_u8_clamped_array_and_sh(
                            Clamped(&image.rgba),
                            image.width,
                            image.height,
                        )
                        .ok()?;
                        canvas.set_width(image.width);
                        canvas.set_height(image.height);
                        context.put_image_data(&data, 0.0, 0.0).ok()
                    })();
                    if drawn.is_some() {
                        computed.set_value(Some(inputs));
                    }
                    status.set(if drawn.is_some() {
                        format!(
                            "Difference computed in {:.0} ms at {} × {} pixels.",
                            js_sys::Date::now() - start,
                            image.width,
                            image.height
                        )
                    } else {
                        "Could not draw the difference image.".to_owned()
                    });
                }
                Err(error) => status.set(error),
            }
        });
    });
    view! {
        <div class="viewer-difference" hidden=move || !visible.get() style="width:100%">
            <canvas node_ref=canvas role="img" aria-label="Simulated: perceptual difference between the two looks" style="display:block;width:100%;height:auto"></canvas>
            <div class="difference-legend" aria-label="Difference legend">
                <div style="height:.75rem;background:linear-gradient(90deg,rgb(0,32,77),rgb(67,78,108),rgb(124,123,120),rgb(188,173,108),rgb(255,233,69))"></div>
                <div class="difference-legend-ticks">
                    {super::CLOSENESS_BANDS.into_iter().enumerate().map(|(index, (lower, level))| {
                        view! {
                            <span class="difference-legend-tick"
                                style=format!("--tick:{}%;--row:{index};--desktop-row:{}", lower.get() * 10.0, index % 2)>
                                <span>{level.label()}</span>
                            </span>
                        }
                    }).collect_view()}
                </div>
            </div>
            <p class="difference-timing" role="status">{move || status.get()}</p>
        </div>
    }
}

/// Simulated LUT renders with comparison modes and a neutral image surround.
#[component]
pub fn Viewer(
    #[prop(into)] lut_a: Signal<Arc<Vec<u8>>>,
    #[prop(into)] scene: Signal<Arc<PreparedScene>>,
    label_a: String,
    #[prop(optional, into)] lut_b: Option<Signal<Arc<Vec<u8>>>>,
    #[prop(optional)] label_b: Option<String>,
    #[prop(optional)] mode: ViewerMode,
    #[prop(optional)] surround: Surround,
    #[prop(optional)] difference: DifferenceSupport,
    #[prop(optional)] on_mode_change: Option<Callback<ViewerMode>>,
    #[prop(default = true)] controls: bool,
) -> impl IntoView {
    let selected_mode = RwSignal::new(if lut_b.is_some() {
        mode
    } else {
        ViewerMode::Single
    });
    let selected_surround = RwSignal::new(surround);
    let _ = difference;
    let show_b = RwSignal::new(false);
    let hold_b = RwSignal::new(false);
    let divider = NodeRef::<Div>::new();
    let cleanup = StoredValue::new_local(None::<js_sys::Function>);
    Effect::new(move |_| {
        if let Some(element) = divider.get() {
            cleanup.set_value(Some(bind_divider(element.as_ref())));
        }
    });
    on_cleanup(move || {
        cleanup.with_value(|cleanup| {
            if let Some(cleanup) = cleanup {
                let _ = cleanup.call0(&JsValue::NULL);
            }
        });
    });
    let a_style = Signal::derive(move || match selected_mode.get() {
        ViewerMode::SideBySide => {
            "display:block;width:50%;height:auto;align-self:center".to_owned()
        }
        ViewerMode::Toggle if show_b.get() ^ hold_b.get() => "display:none".to_owned(),
        ViewerMode::Difference => "display:none".to_owned(),
        _ => "display:block;width:100%;height:auto".to_owned(),
    });
    let b_style = Signal::derive(move || {
        match selected_mode.get() {
            ViewerMode::Split => "position:absolute;inset:0;width:100%;height:100%;clip-path:inset(0 0 0 var(--split,50%))".to_owned(),
            ViewerMode::SideBySide => "display:block;width:50%;height:auto;align-self:center".to_owned(),
            ViewerMode::Toggle if show_b.get() ^ hold_b.get() => "display:block;width:100%;height:auto".to_owned(),
            _ => "display:none".to_owned(),
        }
    });
    view! {
        <figure class="viewer">
            <div class="viewer-controls" role="group" aria-label="Comparison view" hidden=!controls || lut_b.is_none()>
                {[ViewerMode::Split, ViewerMode::SideBySide, ViewerMode::Toggle, ViewerMode::Difference].into_iter().map(|mode| view! {
                    <button type="button" aria-pressed=move || (selected_mode.get() == mode).to_string()
                        on:click=move |_| {
                            selected_mode.set(mode);
                            if let Some(callback) = on_mode_change { callback.run(mode); }
                        }>{mode.label()}</button>
                }).collect_view()}
            </div>
            <div class="viewer-surround" role="group" aria-label="Image surround" hidden=!controls>
                {[(Surround::MidGray119, "Mid gray"), (Surround::Black, "Black"), (Surround::White, "White")].into_iter().map(|(surround, label)| view! {
                    <button type="button" aria-pressed=move || (selected_surround.get() == surround).to_string()
                        on:click=move |_| selected_surround.set(surround)>{label}</button>
                }).collect_view()}
            </div>
            <div class="viewer-surround-frame" style=move || format!("padding:1rem;background:{}", selected_surround.get().color())>
                <div class="viewer-images" style="position:relative;display:flex;--split:50%;min-height:3rem">
                    <PreviewCanvas lut=Signal::derive(move || Some(lut_a.get())) scene=scene label=format!("Simulated: {label_a}") style=a_style/>
                    {lut_b.map(|lut_b| view! {
                        <PreviewCanvas lut=Signal::derive(move || Some(lut_b.get())) scene=scene
                            label=format!("Simulated: {}", label_b.unwrap_or_else(|| "Look B".to_owned())) style=b_style/>
                    })}
                    <div node_ref=divider role="slider" tabindex="0" aria-label="Comparison divider"
                        aria-valuemin="0" aria-valuemax="100" aria-valuenow="50" aria-orientation="horizontal"
                        style=move || if selected_mode.get() == ViewerMode::Split {
                            concat!(
                                "position:absolute;left:var(--split,50%);top:0;bottom:0;",
                                "width:2.75rem;transform:translateX(-50%);cursor:ew-resize;",
                                "touch-action:none;background:linear-gradient(90deg,",
                                "transparent calc(50% - 2px),#000 calc(50% - 2px),",
                                "#fff 50%,#fff calc(50% + 2px),",
                                "transparent calc(50% + 2px))"
                            ).to_owned()
                        } else {
                            "display:none".to_owned()
                        }>
                    </div>
                    {lut_b.map(|lut_b| view! {
                        <DifferenceCanvas lut_a lut_b scene visible=Signal::derive(move || selected_mode.get() == ViewerMode::Difference)/>
                    })}
                </div>
            </div>
            <div hidden=move || selected_mode.get() != ViewerMode::Toggle>
                <button type="button" aria-pressed=move || show_b.get().to_string() on:click=move |_| show_b.update(|value| *value = !*value)>"Switch look"</button>
                <button type="button" on:pointerdown=move |_| hold_b.set(true) on:pointerup=move |_| hold_b.set(false)
                    on:pointercancel=move |_| hold_b.set(false) on:pointerleave=move |_| hold_b.set(false)
                    on:keydown=move |event| { if event.key() == " " || event.key() == "Enter" { event.prevent_default(); hold_b.set(true); } }
                    on:keyup=move |_| hold_b.set(false) on:blur=move |_| hold_b.set(false)>"Hold to flip looks"</button>
            </div>
            <figcaption>"Simulated: looks applied to a sample photo by our model."</figcaption>
        </figure>
    }
}

#[derive(Clone, Debug)]
pub enum LookOrigin {
    BuiltIn { cameras: Vec<String> },
    Custom { author: String },
}

impl LookOrigin {
    fn label(&self) -> String {
        match self {
            Self::BuiltIn { cameras } if cameras.is_empty() => "Built-in look".to_owned(),
            Self::BuiltIn { cameras } => format!("Built into {}", cameras.join(", ")),
            Self::Custom { author } => format!("Custom look by {author}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct LookCardItem {
    pub slug: Slug,
    pub version: LookVersion,
    pub name: String,
    pub origin: LookOrigin,
    pub character: String,
    pub status: LookStatus,
    pub lut: Option<Arc<Vec<u8>>>,
}

/// A look's thumbnail, rendered when visible, with its origin and publication status.
#[component]
pub fn LookCard(
    item: LookCardItem,
    #[prop(optional)] scene: Option<Arc<PreparedScene>>,
    #[prop(optional)] on_select: Option<Callback<LookCardItem>>,
) -> impl IntoView {
    let has_scene = scene.is_some();
    let root = NodeRef::<Div>::new();
    let visible = RwSignal::new(false);
    let lut = RwSignal::new(item.lut.clone());
    let error = RwSignal::new(None::<String>);
    let observer = StoredValue::new_local(None::<JsValue>);
    Effect::new(move |_| {
        let Some(element) = root.get() else {
            return;
        };
        let observation = observe_visible(element.as_ref());
        let promise = js_sys::Reflect::get(&observation, &"promise".into())
            .ok()
            .and_then(|value| value.dyn_into::<js_sys::Promise>().ok());
        observer.set_value(Some(observation));
        if let Some(promise) = promise {
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(value) = wasm_bindgen_futures::JsFuture::from(promise).await
                    && value.as_bool() == Some(true)
                {
                    let _ = visible.try_set(true);
                }
            });
        }
    });
    on_cleanup(move || {
        observer.with_value(|observation| {
            if let Some(observation) = observation
                && let Ok(cancel) = js_sys::Reflect::get(observation, &"cancel".into())
                && let Some(cancel) = cancel.dyn_ref::<js_sys::Function>()
            {
                let _ = cancel.call0(&JsValue::NULL);
            }
        })
    });
    let slug = item.slug.clone();
    let version = item.version.clone();
    Effect::new(move |_| {
        if !has_scene || !visible.get() || lut.get_untracked().is_some() {
            return;
        }
        let slug = slug.clone();
        let version = version.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::data::preview(slug.as_str(), version.as_str()).await {
                Ok(bytes) => {
                    let _ = lut.try_set(Some(bytes));
                }
                Err(cause) => {
                    let _ = error.try_set(Some(cause));
                }
            }
        });
    });
    let name = item.name.clone();
    let origin = item.origin.label();
    let character = item.character.clone();
    let select = item.clone();
    view! {
        <div node_ref=root class="look-card">
            <button type="button"
                on:click=move |_| {
                    if let Some(callback) = on_select {
                        callback.run(select.clone());
                    } else {
                        let _ = window().location().set_href(&crate::base::site_url(&format!("/looks/{}", select.slug)));
                    }
                }>
                {scene.map(|scene| view! {
                    <div style="background:rgb(119,119,119);min-height:5rem">
                        <PreviewCanvas lut=Signal::derive(move || if visible.get() { lut.get() } else { None })
                            scene=Signal::stored(scene) label=format!("Simulated: {name}")
                            style=Signal::stored("display:block;width:100%;height:auto".to_owned())/>
                    </div>
                })}
                <strong>{name}</strong>
                <span>{origin}</span>
                <super::Badge status=item.status/>
                <span>{character}</span>
                <span class="muted">"Simulated"</span>
            </button>
            {move || error.get().map(|message| view! { <p role="alert">{message}</p> })}
        </div>
    }
}

/// A searchable popover gallery with lazy thumbnail rendering.
#[component]
pub fn LookPicker(
    items: Vec<LookCardItem>,
    #[prop(optional_no_strip, into)] scene: Option<Arc<PreparedScene>>,
    on_select: Callback<LookCardItem>,
    #[prop(optional)] label: Option<String>,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let trigger = NodeRef::<leptos::html::Button>::new();
    let input = NodeRef::<leptos::html::Input>::new();
    Effect::new(move |_| {
        if open.get()
            && let Some(input) = input.get()
        {
            let _ = input.focus();
        }
    });
    let select = Callback::new(move |item| {
        on_select.run(item);
        open.set(false);
        if let Some(trigger) = trigger.get() {
            let _ = trigger.focus();
        }
    });
    view! {
        <div class="look-picker" style="position:relative" on:keydown=move |event| {
            if event.key() == "Escape" && open.get() {
                event.prevent_default(); open.set(false);
                if let Some(trigger) = trigger.get() { let _ = trigger.focus(); }
            }
        }>
            <button node_ref=trigger type="button" aria-haspopup="dialog" aria-expanded=move || open.get().to_string()
                on:click=move |_| open.update(|value| *value = !*value)>{label.unwrap_or_else(|| "Choose a look".to_owned())}</button>
            <div class="look-picker-popover" role="dialog" aria-label="Choose a look" hidden=move || !open.get()
                style="position:absolute;z-index:10;inset-inline-start:0;width:min(42rem,calc(100vw - 3rem));padding:1rem;background:var(--panel);color:var(--ink);border:1px solid var(--line)">
                <label>"Search looks"
                    <input node_ref=input type="search" prop:value=move || query.get()
                        on:input=move |event| query.set(event_target_value(&event))/>
                </label>
                <button type="button" on:click=move |_| {
                    open.set(false); if let Some(trigger) = trigger.get() { let _ = trigger.focus(); }
                }>"Close"</button>
                <div class="look-picker-gallery">
                    {items.into_iter().map(|item| {
                        let search = format!("{} {} {}", item.name, item.origin.label(), item.character).to_lowercase();
                        view! {
                            <div hidden=move || !search.contains(&query.get().to_lowercase())>
                                {match &scene {
                                    Some(scene) => view! {
                                        <LookCard item=item scene=Arc::clone(scene) on_select=select/>
                                    }.into_any(),
                                    None => view! {
                                        <LookCard item=item on_select=select/>
                                    }.into_any(),
                                }}
                            </div>
                        }
                    }).collect_view()}
                </div>
            </div>
        </div>
    }
}
