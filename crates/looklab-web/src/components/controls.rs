// SPDX-License-Identifier: AGPL-3.0-or-later

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_model::LookKind;
use looklab_publish::{BodyKind, BodySummary, LookStatus};
use looklab_schema::BodyId;
use wasm_bindgen::{JsCast, JsValue, prelude::wasm_bindgen};
use wasm_bindgen_futures::JsFuture;
use web_sys::{File, HtmlInputElement};

static NEXT_TABS_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_CAMERA_ID: AtomicUsize = AtomicUsize::new(0);
static NEXT_STEPPER_ID: AtomicUsize = AtomicUsize::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CameraOptionIndex(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CameraKey {
    Next,
    Previous,
    First,
    Last,
    Select,
    Dismiss,
    Other,
}

impl From<&str> for CameraKey {
    fn from(value: &str) -> Self {
        match value {
            "ArrowDown" => Self::Next,
            "ArrowUp" => Self::Previous,
            "Home" => Self::First,
            "End" => Self::Last,
            "Enter" => Self::Select,
            "Escape" => Self::Dismiss,
            _ => Self::Other,
        }
    }
}

#[wasm_bindgen(inline_js = r#"
export function controlsDialog(root, open) {
    const dialog = root.querySelector('dialog');
    if (!dialog.dataset.focusTrap) {
        dialog.dataset.focusTrap = 'true';
        dialog.addEventListener('keydown', event => {
            if (event.key !== 'Tab') return;
            const controls = Array.from(dialog.querySelectorAll('a[href],button,input,select,textarea,[tabindex]'))
                .filter(element => !element.disabled && element.tabIndex >= 0 && element.getClientRects().length);
            const first = controls[0];
            const last = controls[controls.length - 1];
            if (!first) {
                event.preventDefault();
                dialog.focus();
            } else if (event.shiftKey && document.activeElement === first) {
                event.preventDefault();
                last.focus();
            } else if (!event.shiftKey && document.activeElement === last) {
                event.preventDefault();
                first.focus();
            }
        });
    }
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) dialog.close();
}
export function controlsFocusTab(root, index) {
    root.querySelectorAll('[role=tab]')[index]?.focus();
}
export function controlsCameraActive(id) {
    document.getElementById(id)?.scrollIntoView({block: 'nearest'});
}
export function controlsDroppedFiles(event) {
    const transfer = event.dataTransfer;
    const entries = Array.from(transfer.items || [], item => item.webkitGetAsEntry?.()).filter(Boolean);
    if (!entries.length) return Promise.resolve(Array.from(transfer.files));
    async function visit(entry) {
        if (entry.isFile) return [await new Promise((resolve, reject) => entry.file(resolve, reject))];
        const reader = entry.createReader();
        const files = [];
        for (;;) {
            const batch = await new Promise((resolve, reject) => reader.readEntries(resolve, reject));
            if (!batch.length) return files;
            for (const child of batch) files.push(...await visit(child));
        }
    }
    return Promise.all(entries.map(visit)).then(groups => groups.flat());
}
"#)]
extern "C" {
    #[wasm_bindgen(catch, js_name = controlsDialog)]
    fn sync_dialog(root: &JsValue, open: bool) -> Result<(), JsValue>;
    #[wasm_bindgen(js_name = controlsFocusTab)]
    fn focus_tab(root: &JsValue, index: usize);
    #[wasm_bindgen(js_name = controlsDroppedFiles)]
    fn dropped_files(event: &JsValue) -> js_sys::Promise;
    #[wasm_bindgen(js_name = controlsCameraActive)]
    fn scroll_camera_option(id: &str);
}

pub fn look_kind_label(kind: LookKind) -> &'static str {
    match kind {
        LookKind::NativeFamily => "Built-in look",
        LookKind::LutDefined => "Custom look",
    }
}

pub fn status_label(status: LookStatus, details: bool) -> Option<&'static str> {
    match status {
        LookStatus::Provisional => Some("Early estimate"),
        LookStatus::Rooted if details => Some("Reference data"),
        LookStatus::Deprecated => Some("Older version"),
        LookStatus::Rooted | LookStatus::Published => None,
    }
}

/// Publication status in photographer-facing words.
#[component]
pub fn Badge(status: LookStatus, #[prop(default = false)] details: bool) -> impl IntoView {
    status_label(status, details).map(|label| view! { <span class="badge">{label}</span> })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClosenessLevel {
    Indistinguishable,
    NearlyIdentical,
    Close,
    NoticeableDifference,
    DifferentLook,
}

impl ClosenessLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Indistinguishable => "Indistinguishable",
            Self::NearlyIdentical => "Nearly identical",
            Self::Close => "Close",
            Self::NoticeableDifference => "Noticeable difference",
            Self::DifferentLook => "Different look",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeltaE(f64);

impl DeltaE {
    pub fn new(value: f64) -> Option<Self> {
        (value.is_finite() && value >= 0.0).then_some(Self(value))
    }

    pub fn get(self) -> f64 {
        self.0
    }
}

pub const CLOSENESS_BANDS: [(DeltaE, ClosenessLevel); 5] = [
    (DeltaE(0.0), ClosenessLevel::Indistinguishable),
    (DeltaE(1.0), ClosenessLevel::NearlyIdentical),
    (DeltaE(2.0), ClosenessLevel::Close),
    (DeltaE(4.0), ClosenessLevel::NoticeableDifference),
    (DeltaE(7.0), ClosenessLevel::DifferentLook),
];

pub fn closeness_level(value: DeltaE) -> ClosenessLevel {
    CLOSENESS_BANDS
        .iter()
        .rev()
        .find(|(lower, _)| value.get() >= lower.get())
        .expect("closeness bands start at zero")
        .1
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstimateConfidence {
    Established,
    EarlyEstimate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ClosenessEstimate {
    median: DeltaE,
    band: [DeltaE; 2],
    p95: DeltaE,
    confidence: EstimateConfidence,
}

impl ClosenessEstimate {
    pub fn new(
        median: f64,
        band: [f64; 2],
        p95: f64,
        confidence: EstimateConfidence,
    ) -> Option<Self> {
        let median = DeltaE::new(median)?;
        let band = [DeltaE::new(band[0])?, DeltaE::new(band[1])?];
        let p95 = DeltaE::new(p95)?;
        (band[0].get() <= median.get()
            && median.get() <= band[1].get()
            && median.get() <= p95.get())
        .then_some(Self {
            median,
            band,
            p95,
            confidence,
        })
    }

    pub fn label(self) -> String {
        let low = closeness_level(self.band[0]);
        let high = closeness_level(self.band[1]);
        let label = if low == high {
            closeness_level(self.median).label().to_owned()
        } else {
            format!("{} to {}", high.label(), low.label().to_lowercase())
        };
        match self.confidence {
            EstimateConfidence::Established => label,
            EstimateConfidence::EarlyEstimate => format!("{label} (early estimate)"),
        }
    }
}

/// Qualitative closeness with uncertainty available by hover, focus or tap.
#[component]
pub fn Closeness(estimate: ClosenessEstimate) -> impl IntoView {
    let hovered = RwSignal::new(false);
    let focused = RwSignal::new(false);
    let expanded = RwSignal::new(false);
    let numbers = format!(
        "Median {:.2}; 90% band {:.2} to {:.2}; p95 {:.2} ΔE (CAM16-UCS).",
        estimate.median.get(),
        estimate.band[0].get(),
        estimate.band[1].get(),
        estimate.p95.get()
    );
    view! {
        <span class="closeness" on:mouseenter=move |_| hovered.set(true) on:mouseleave=move |_| hovered.set(false)>
            <button type="button" class="closeness-label" aria-expanded=move || expanded.get().to_string()
                on:focus=move |_| focused.set(true) on:blur=move |_| focused.set(false)
                on:click=move |_| expanded.update(|value| *value = !*value) title=numbers.clone()>
                {estimate.label()}
            </button>
            <span class="closeness-details" role="status" hidden=move || !(hovered.get() || focused.get() || expanded.get())>{numbers}</span>
        </span>
    }
}

/// A grouped camera combobox with local JPEG detection.
#[component]
pub fn CameraPicker(
    cameras: Vec<BodySummary>,
    selected: RwSignal<Option<BodyId>>,
    #[prop(default = "Your camera".to_owned())] label: String,
) -> impl IntoView {
    let mut cameras = cameras
        .into_iter()
        .filter(|camera| camera.kind == BodyKind::Camera)
        .collect::<Vec<_>>();
    cameras.sort_by(|left, right| (&left.make, &left.model).cmp(&(&right.make, &right.model)));
    let cameras = StoredValue::new(cameras);
    let prefix = format!(
        "camera-picker-{}",
        NEXT_CAMERA_ID.fetch_add(1, Ordering::Relaxed)
    );
    let input_id = format!("{prefix}-input");
    let list_id = format!("{prefix}-list");
    let message_id = format!("{prefix}-message");
    let option_prefix = StoredValue::new(prefix);
    let photo_input = NodeRef::<leptos::html::Input>::new();
    let query = RwSignal::new(String::new());
    let open = RwSignal::new(false);
    let filtering = RwSignal::new(false);
    let active = RwSignal::new(None::<CameraOptionIndex>);
    let message = RwSignal::new(None::<String>);
    let restore = move || {
        query.set(cameras.with_value(|cameras| {
            cameras
                .iter()
                .find(|camera| Some(&camera.id) == selected.get_untracked().as_ref())
                .map(|camera| format!("{} {}", camera.make, camera.model))
                .unwrap_or_default()
        }));
        filtering.set(false);
    };
    Effect::new(move |_| {
        let current = selected.get();
        query.set(cameras.with_value(|cameras| {
            cameras
                .iter()
                .find(|camera| Some(&camera.id) == current.as_ref())
                .map(|camera| format!("{} {}", camera.make, camera.model))
                .unwrap_or_default()
        }));
        filtering.set(false);
    });
    let matches = Memo::new(move |_| {
        let search = if filtering.get() {
            query.get().to_lowercase()
        } else {
            String::new()
        };
        cameras.with_value(|cameras| {
            cameras
                .iter()
                .enumerate()
                .filter(|(_, camera)| {
                    search.is_empty()
                        || format!("{} {}", camera.make, camera.model)
                            .to_lowercase()
                            .contains(&search)
                })
                .map(|(index, _)| CameraOptionIndex(index))
                .collect::<Vec<_>>()
        })
    });
    Effect::new(move |_| {
        let matches = matches.get();
        let current = selected.get();
        active.set(
            matches
                .iter()
                .copied()
                .find(|index| {
                    cameras.with_value(|cameras| Some(&cameras[index.0].id) == current.as_ref())
                })
                .or_else(|| matches.first().copied()),
        );
    });
    Effect::new(move |_| {
        if open.get()
            && let Some(index) = active.get()
        {
            option_prefix
                .with_value(|prefix| scroll_camera_option(&format!("{prefix}-option-{}", index.0)));
        }
    });
    let choose = Callback::new(move |index: CameraOptionIndex| {
        selected.set(
            cameras.with_value(|cameras| cameras.get(index.0).map(|camera| camera.id.clone())),
        );
        restore();
        open.set(false);
    });
    let detect = Callback::new(move |file: File| {
        message.set(Some(
            "Reading camera information on this device…".to_owned(),
        ));
        open.set(false);
        spawn_local(async move {
            let result = async {
                let buffer = JsFuture::from(file.array_buffer()).await.map_err(|_| "Could not read this photo. Choose a JPEG from your camera.".to_owned())?;
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                if !bytes.starts_with(&[0xff, 0xd8]) { return Err("Choose a JPEG photo from your camera.".to_owned()); }
                let plan = looklab_wasm::inspect_files(&[(file.name(), bytes)]).map_err(|_| "Could not read camera information. Choose your camera by name.".to_owned())?;
                let fact = plan.facts.first().ok_or_else(|| "No camera information found. Choose your camera by name.".to_owned())?;
                let (Some(make), Some(model)) = (&fact.make, &fact.model) else { return Err("No camera information found. Choose your camera by name.".to_owned()); };
                let identity = looklab_prep::CameraIdentity::new(make, model);
                cameras.with_value(|cameras| cameras.iter().find(|camera| identity.as_ref().is_some_and(|identity| identity.matches(&camera.make, &camera.model))).map(|camera| (camera.id.clone(), format!("Detected {} {}. Nothing was uploaded.", camera.make, camera.model))))
                    .ok_or_else(|| format!("Detected {make} {model}, which is not ready for matching yet. Choose a supported camera."))
            }.await;
            match result {
                Ok((id, note)) => {
                    selected.set(Some(id));
                    restore();
                    message.set(Some(note));
                }
                Err(error) => message.set(Some(error)),
            }
        });
    });
    view! {
        <div class="camera-picker">
            <label for=input_id.clone()>{label.clone()}</label>
            <div class="camera-picker-row">
                <div class="camera-combobox">
                    <input id=input_id type="text" role="combobox" autocomplete="off"
                        aria-autocomplete="list" aria-haspopup="listbox" aria-expanded=move || open.get().to_string()
                        aria-controls=list_id.clone() aria-describedby=message_id.clone()
                        aria-activedescendant=move || if open.get() { active.get().map(|index| option_prefix.with_value(|prefix| format!("{prefix}-option-{}", index.0))) } else { None }
                        placeholder="Search cameras" prop:value=move || query.get()
                        on:focus=move |_| open.set(true)
                        on:click=move |_| open.set(true)
                        on:blur=move |_| { open.set(false); restore(); }
                        on:input=move |event| { query.set(event_target_value(&event)); filtering.set(true); open.set(true); }
                        on:keydown=move |event| {
                            let key = CameraKey::from(event.key().as_str());
                            match key {
                                CameraKey::Next | CameraKey::Previous | CameraKey::First | CameraKey::Last => {
                                    if !open.get() && matches!(key, CameraKey::First | CameraKey::Last) { return; }
                                    event.prevent_default();
                                    let was_open = open.get();
                                    open.set(true);
                                    matches.with(|matches| {
                                        if matches.is_empty() { active.set(None); return; }
                                        let current = matches.iter().position(|index| Some(*index) == active.get_untracked());
                                        let next = match key {
                                            CameraKey::First => 0,
                                            CameraKey::Last => matches.len() - 1,
                                            CameraKey::Previous if !was_open => matches.len() - 1,
                                            CameraKey::Next if !was_open => 0,
                                            CameraKey::Previous => current.unwrap_or(0).saturating_sub(1),
                                            _ => current.map(|index| (index + 1).min(matches.len() - 1)).unwrap_or(0),
                                        };
                                        active.set(Some(matches[next]));
                                    });
                                }
                                CameraKey::Select if open.get() => {
                                    event.prevent_default();
                                    if let Some(index) = active.get() { choose.run(index); }
                                }
                                CameraKey::Dismiss if open.get() => { event.prevent_default(); event.stop_propagation(); open.set(false); restore(); }
                                _ => {}
                            }
                        }
                        on:dragover=move |event| event.prevent_default()
                        on:drop=move |event| {
                            event.prevent_default();
                            let pending = dropped_files(event.as_ref());
                            spawn_local(async move {
                                match JsFuture::from(pending).await {
                                    Ok(files) => {
                                        if let Some(file) = js_sys::Array::from(&files).iter().find_map(|file| file.dyn_into::<File>().ok()) { detect.run(file); }
                                    }
                                    Err(_) => message.set(Some("Could not read this photo. Use Detect from a photo instead.".to_owned())),
                                }
                            });
                        }/>
                    <div id=list_id role="listbox" aria-label=label class="camera-listbox" hidden=move || !open.get()>
                        {move || matches.with(|matches| cameras.with_value(|cameras| {
                            let mut groups = BTreeMap::<&str, Vec<CameraOptionIndex>>::new();
                            for index in matches { groups.entry(&cameras[index.0].make).or_default().push(*index); }
                            groups.into_iter().map(|(make, indices)| {
                                let group_id = option_prefix.with_value(|prefix| format!("{prefix}-make-{}", indices[0].0));
                                view! {
                                    <div role="group" aria-labelledby=group_id>
                                        <div id=group_id.clone() class="camera-make">{make.to_owned()}</div>
                                        {indices.into_iter().map(|index| {
                                            let camera = &cameras[index.0];
                                            let id = camera.id.clone();
                                            view! {
                                                <div role="option" id=option_prefix.with_value(|prefix| format!("{prefix}-option-{}", index.0))
                                                    aria-selected=move || (selected.get().as_ref() == Some(&id)).to_string()
                                                    class="camera-option" class:active=move || active.get() == Some(index)
                                                    on:pointerdown=move |event| event.prevent_default()
                                                    on:click=move |_| choose.run(index)>
                                                    {format!("{} {}", camera.make, camera.model)}
                                                </div>
                                            }
                                        }).collect_view()}
                                    </div>
                                }
                            }).collect_view()
                        }))}
                        <p class="camera-no-results" hidden=move || !matches.get().is_empty()>"No matching cameras."</p>
                    </div>
                </div>
                <input node_ref=photo_input type="file" accept="image/jpeg,.jpg,.jpeg" hidden
                    on:change=move |event| { if let Some(file) = files_from_input(&event).into_iter().next() { detect.run(file); } }/>
                <button type="button" class="camera-detect" on:click=move |_| { if let Some(input) = photo_input.get() { input.click(); } }>"Detect from a photo"</button>
            </div>
            <p id=message_id class="camera-picker-message" role="status">{move || message.get()}</p>
        </div>
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StepIndex(pub usize);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WizardStep {
    pub title: String,
    pub summary: Option<String>,
}

#[component]
pub fn Stepper(
    steps: Vec<WizardStep>,
    current: RwSignal<StepIndex>,
    furthest: RwSignal<StepIndex>,
    #[prop(into, default = Signal::derive(|| true))] can_advance: Signal<bool>,
    /// While this holds `Some(limit)`, steps after `limit` stay unreachable.
    #[prop(into, default = Signal::derive(|| None))]
    blocked_beyond: Signal<Option<usize>>,
    #[prop(into, default = Signal::derive(|| false))] locked: Signal<bool>,
    #[prop(into, default = Signal::derive(|| false))] finished: Signal<bool>,
    #[prop(into, default = Signal::derive(Vec::new))] summaries: Signal<Vec<Option<String>>>,
    #[prop(optional)] children: Option<Children>,
    #[prop(optional)] on_finish: Option<Callback<()>>,
    #[prop(default = "Finish".to_owned())] finish_label: String,
) -> impl IntoView {
    let count = steps.len();
    let instance = NEXT_STEPPER_ID.fetch_add(1, Ordering::Relaxed);
    let final_step = Signal::derive(move || current.get().0.saturating_add(1) >= count);
    Effect::new(move |_| {
        let step = current.get();
        furthest.update(|furthest| furthest.0 = furthest.0.max(step.0));
    });
    view! {
        <div class="stepper">
            <nav aria-label="Progress">
                <ol>{steps.into_iter().enumerate().map(|(index, step)| {
                    let title = step.title.clone();
                    let summary = Signal::derive(move || {
                        summaries.with(|summaries| summaries.get(index).cloned().flatten())
                            .or_else(|| step.summary.clone())
                    });
                    let complete = move || index < furthest.get().0;
                    let state = move || {
                        if current.get().0 == index {
                            "current"
                        } else if complete() {
                            "complete"
                        } else {
                            "upcoming"
                        }
                    };
                    let marker = Signal::derive(move || {
                        if complete() { "✓".to_owned() } else { (index + 1).to_string() }
                    });
                    let summary_id = format!("stepper-{instance}-step-{index}");
                    let described_by = summary_id.clone();
                    view! {
                        <li class="step-item" data-state=state
                            aria-current=move || { (current.get().0 == index).then_some("step") }>
                            <button
                                type="button"
                                class="step-link"
                                aria-describedby=move || complete().then(|| described_by.clone())
                                disabled=move || {
                                    locked.get()
                                        || index > furthest.get().0
                                        || blocked_beyond.get().is_some_and(|limit| index > limit)
                                }
                                on:click=move |_| {
                                    if !locked.get_untracked() { current.set(StepIndex(index)); }
                                }
                            >
                                <span class="step-marker" aria-hidden="true">{marker}</span>
                                <span class="step-title">{title}</span>
                            </button>
                            {move || complete().then(|| view! {
                                <span class="step-summary" id=summary_id.clone()>
                                    <span class="visually-hidden">"Completed. "</span>
                                    {move || summary.get().unwrap_or_default()}
                                </span>
                            })}
                        </li>
                    }
                }).collect_view()}</ol>
            </nav>
            {children.map(|children| children())}
            {move || (!finished.get()).then(|| {
                let finish_label = finish_label.clone();
                view! {
                <div class="stepper-actions">
                    <button
                        type="button"
                        disabled=move || locked.get() || current.get().0 == 0
                        on:click=move |_| {
                            if !locked.get_untracked() {
                                current.update(|step| step.0 = step.0.saturating_sub(1));
                            }
                        }
                    >
                        "Back"
                    </button>
                    <button type="button" disabled=move || {
                        locked.get()
                            || !can_advance.get()
                            || count == 0
                            || (final_step.get() && on_finish.is_none())
                            || blocked_beyond
                                .get()
                                .is_some_and(|limit| current.get().0.saturating_add(1) > limit)
                    }
                        on:click=move |_| {
                            if locked.get_untracked() { return; }
                            let next = current.get_untracked().0.saturating_add(1);
                            if next < count {
                                current.set(StepIndex(next));
                            } else if count > 0
                                && let Some(finish) = on_finish
                            {
                                finish.run(());
                            }
                        }>
                        {move || if final_step.get() && on_finish.is_some() { finish_label.clone() } else { "Next".to_owned() }}
                    </button>
                </div>
                }
            })}
        </div>
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TabId(String);

impl TabId {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'))
        .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabSpec {
    pub id: TabId,
    pub label: String,
}

/// Keyboard-navigable tabs around caller-rendered active content.
#[component]
pub fn Tabs(tabs: Vec<TabSpec>, selected: RwSignal<TabId>, children: Children) -> impl IntoView {
    let root = NodeRef::<leptos::html::Div>::new();
    let tabs = StoredValue::new(tabs);
    let prefix = format!("tabs-{}", NEXT_TABS_ID.fetch_add(1, Ordering::Relaxed));
    let panel_id = format!("{prefix}-panel");
    let labelled_prefix = prefix.clone();
    view! {
        <div class="tabs" node_ref=root>
            <div role="tablist" aria-label="Page sections">
                {tabs.with_value(|items| items.iter().enumerate().map(|(index, tab)| {
                    let id = tab.id.clone();
                    let active_id = id.clone();
                    let focus_id = id.clone();
                    view! {
                        <button type="button" role="tab" id=format!("{prefix}-{}", id.as_str()) aria-controls=panel_id.clone()
                            aria-selected=move || (selected.get() == active_id).to_string()
                            tabindex=move || if selected.get() == focus_id { 0 } else { -1 }
                            on:click=move |_| selected.set(id.clone())
                            on:keydown=move |event| {
                                let count = tabs.with_value(Vec::len);
                                if count == 0 { return; }
                                let next = match event.key().as_str() {
                                    "ArrowRight" => (index + 1) % count,
                                    "ArrowLeft" => (index + count - 1) % count,
                                    "Home" => 0,
                                    "End" => count - 1,
                                    _ => return,
                                };
                                event.prevent_default();
                                selected.set(tabs.with_value(|tabs| tabs[next].id.clone()));
                                if let Some(root) = root.get() { focus_tab(root.as_ref(), next); }
                            }>{tab.label.clone()}</button>
                    }
                }).collect_view())}
            </div>
            <div role="tabpanel" id=panel_id tabindex="0" aria-labelledby=move || format!("{labelled_prefix}-{}", selected.get().as_str())>{children()}</div>
        </div>
    }
}

/// Native disclosure for technical details.
#[component]
pub fn Disclosure(
    summary: String,
    children: Children,
    #[prop(default = false)] open: bool,
    #[prop(optional)] id: Option<String>,
) -> impl IntoView {
    view! { <details class="disclosure" id=id open=open><summary>{summary}</summary>{children()}</details> }
}

/// Native modal dialog with browser focus containment and Escape dismissal.
#[component]
pub fn Dialog(title: String, open: RwSignal<bool>, children: Children) -> impl IntoView {
    let root = NodeRef::<leptos::html::Div>::new();
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let visible = open.get();
        if let Some(root) = root.get()
            && let Err(cause) = sync_dialog(root.as_ref(), visible)
        {
            error.set(Some(format!("Could not open this dialog: {cause:?}")));
        }
    });
    view! {
        <div node_ref=root>
            <dialog
                class="dialog"
                aria-label=title.clone()
                on:cancel=move |_: web_sys::Event| open.set(false)
                on:close=move |event| {
                    if let Some(dialog) = event.target().and_then(|target| target.dyn_into::<web_sys::Element>().ok())
                        && dialog.is_connected()
                        && !dialog.has_attribute("open")
                    {
                        open.set(false);
                    }
                }
            >
                <header><h2>{title.clone()}</h2><button type="button" aria-label="Close dialog" on:click=move |_| open.set(false)>"Close"</button></header>
                {children()}
            </dialog>
            {move || error.get().map(|error| view! { <p role="alert">{error}</p> })}
        </div>
    }
}

fn files_from_input(event: &leptos::ev::Event) -> Vec<File> {
    let Some(input) = event
        .target()
        .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
    else {
        return Vec::new();
    };
    let files = input
        .files()
        .map(|files| {
            (0..files.length())
                .filter_map(|index| files.get(index))
                .collect()
        })
        .unwrap_or_default();
    input.set_value("");
    files
}

/// Local file and folder selection with recursive directory drops.
#[component]
pub fn DropZone(
    on_files: Callback<Vec<File>>,
    #[prop(default = String::new())] accept: String,
    #[prop(default = true)] folders: bool,
    #[prop(default = "Drop files or a folder here. Files stay on this device until you choose to send.".to_owned())]
    prompt: String,
    #[prop(into, default = Signal::derive(|| false))] disabled: Signal<bool>,
    #[prop(optional)] file_picker: Option<NodeRef<leptos::html::Input>>,
) -> impl IntoView {
    let files = file_picker.unwrap_or_default();
    let directory = NodeRef::<leptos::html::Input>::new();
    let dragging = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    view! {
        <section class="drop-zone" class:dragging=move || dragging.get() aria-label="Add files from this device"
            on:dragover=move |event| { event.prevent_default(); if !disabled.get() { dragging.set(true); } }
            on:dragleave=move |_| dragging.set(false)
            on:drop=move |event| {
                event.prevent_default();
                dragging.set(false);
                if disabled.get() { return; }
                let pending = dropped_files(event.as_ref());
                spawn_local(async move {
                    match JsFuture::from(pending).await {
                        Ok(value) => {
                            error.set(None);
                            on_files.run(js_sys::Array::from(&value).iter().filter_map(|file| file.dyn_into::<File>().ok()).collect());
                        }
                        Err(cause) => error.set(Some(format!("Could not read these files. Try Choose files instead: {cause:?}"))),
                    }
                });
            }>
            <p>{prompt}</p>
            <input node_ref=files type="file" multiple accept=accept hidden disabled=move || disabled.get()
                on:change=move |event| { if !disabled.get() { on_files.run(files_from_input(&event)); } }/>
            <div class="field-row">
                <button type="button" disabled=move || disabled.get() on:click=move |_| { if let Some(input) = files.get() { input.click(); } }>"Choose files"</button>
                {folders.then(|| view! {
                    <input node_ref=directory type="file" multiple prop:webkitdirectory=true hidden disabled=move || disabled.get()
                        on:change=move |event| { if !disabled.get() { on_files.run(files_from_input(&event)); } }/>
                    <button type="button" disabled=move || disabled.get() on:click=move |_| { if let Some(input) = directory.get() { input.click(); } }>"Choose folder"</button>
                })}
            </div>
            {move || error.get().map(|error| view! { <p role="alert">{error}</p> })}
        </section>
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NoticeKind {
    #[default]
    Information,
    Warning,
    Error,
}

/// A plain-language notice with optional local dismissal.
#[component]
pub fn Notice(
    message: String,
    #[prop(default = NoticeKind::Information)] kind: NoticeKind,
    #[prop(default = false)] dismissible: bool,
) -> impl IntoView {
    let dismissed = RwSignal::new(false);
    let class = match kind {
        NoticeKind::Information => "notice",
        NoticeKind::Warning => "notice notice-warning",
        NoticeKind::Error => "notice notice-error",
    };
    view! {
        <aside class=class role=if kind == NoticeKind::Error { "alert" } else { "status" } hidden=move || dismissed.get()>
            <p>{message}</p>
            {dismissible.then(|| view! { <button type="button" aria-label="Dismiss notice" on:click=move |_| dismissed.set(true)>"Dismiss"</button> })}
        </aside>
    }
}

/// An empty state with a specific explanation and caller-supplied next action.
#[component]
pub fn EmptyState(
    title: String,
    message: String,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! { <section class="empty-state"><h2>{title}</h2><p>{message}</p>{children.map(|children| children())}</section> }
}
