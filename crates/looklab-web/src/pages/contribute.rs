// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local contribution flow and payload inspector ([docs/08 §4](../../../docs/08-ui.md)).

use leptos::html::Canvas;
use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_meta::ParsedMetadata;
use looklab_prep::LocalImageKey;
use looklab_upload::{
    CardDeclaration, ContributionKind, FirmwareId, FirmwareVersion, MonochromeAuthorization,
    RenderClass, RenderTag, SettingSource,
};
use looklab_wasm::coverage::{CoverageMap, coverage_map, sample_points};
use looklab_wasm::prep::{ModuleFunction, default_pow};
use looklab_wasm::{
    CardCheckError, ControlSetting, GroupSettings, IntentChoice, PairingPlan, PowParamsSpec,
    PrepSummary, PreparedImageView, build_manifest_request, build_send_manifest_request,
    check_card, decode_build_manifest_reply, decode_build_send_manifest_reply, decode_encode_reply,
    decode_pow_reply, decode_prepare_reply, encode_envelope_request, inspect_metadata,
    module_fn_id, prefill_settings_parsed, prepare_request, serial_ascii_from_metadata,
    solve_pow_request,
};
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use super::bounties::{Impact, WantedCards};
use crate::data;
use crate::files::{
    ContributionDraftStore, DraftFile, LocalFile, download, draft_file_entries, read_draft_file,
    scan_metadata, selected_files,
};
use crate::ui::{Dialog, Disclosure, DropZone, StepIndex, Stepper, WizardStep};
use crate::worker::{PrepWorker, default_worker_count, max_worker_count};
use crate::{idb, webauthn};

const PRIOR_VERSION: u32 = 1;

/// Shown for a B&W render whose batch did not select monochrome model development.
const MONOCHROME_UNSELECTED: &str =
    "B&W render left out: monochrome model development is not selected for this batch";

/// Identifies one upload manifest within a drop.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
pub(crate) struct BatchId(u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RenderFile(LocalImageKey);

impl RenderFile {
    fn new(name: &str) -> Option<Self> {
        let key = LocalImageKey::new(name)?;
        (key.kind() == looklab_prep::FileKind::Jpeg).then_some(Self(key))
    }

    fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl serde::Serialize for RenderFile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for RenderFile {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <String as serde::Deserialize>::deserialize(deserializer)?;
        Self::new(&name).ok_or_else(|| serde::de::Error::custom("expected a JPEG file name"))
    }
}

/// The manifest fields the confirm step edits; one set per batch.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct GroupFields {
    pub(crate) body: String,
    pub(crate) firmware: String,
    pub(crate) sim: String,
    pub(crate) controls: String,
    pub(crate) recipe: bool,
    pub(crate) target: String,
    pub(crate) card: bool,
    pub(crate) card_firmware_id: String,
    pub(crate) card_firmware_version: String,
    /// The contributor selected this batch's B&W renders for monochrome model development.
    pub(crate) monochrome_opt_in: bool,
}

/// One manifest: renders sharing a body, firmware version, look and settings.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub(crate) struct ContributionGroup {
    pub(crate) id: BatchId,
    pub(crate) raws: Vec<String>,
    pub(crate) renders: Vec<RenderFile>,
    pub(crate) fields: GroupFields,
    pub(crate) serial_opt_in: bool,
    pub(crate) malformed: bool,
    pub(crate) unavailable: Vec<String>,
    pub(crate) notes: Vec<String>,
    /// Controls the makernotes reproduced; every other control in the settings is declared.
    pub(crate) reproduced: Vec<(String, i64)>,
    /// Look-affecting controls the schema maps from no tag; each needs a declared value.
    pub(crate) unmapped: Vec<String>,
    /// The schema's normalized family for this batch's native sim.
    pub(crate) family: Option<looklab_schema::LookId>,
    /// The normalized filter for a monochrome family; Sepia carries toning, not a filter.
    pub(crate) mono_filter: Option<looklab_schema::MonoFilter>,
    /// The batch's renders the schema classifies as monochrome.
    pub(crate) monochrome: Vec<looklab_wasm::MonochromeRender>,
}

/// The batches one drop uploads.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct BatchSet {
    groups: Vec<ContributionGroup>,
    next: u64,
}

impl BatchSet {
    fn from_groups(groups: Vec<ContributionGroup>) -> Self {
        let next = groups.iter().map(|group| group.id.0).max().unwrap_or(0);
        Self { groups, next }
    }

    /// Splits renders by body, firmware version, look, settings and card declaration.
    fn from_readings(readings: &[PairReading]) -> Self {
        let mut set = Self::default();
        let mut seen: HashMap<GroupKey, usize> = HashMap::new();
        for reading in readings {
            if let Some(&index) = seen.get(&reading.key()) {
                if !set.groups[index].raws.contains(&reading.raw) {
                    set.groups[index].raws.push(reading.raw.clone());
                }
                set.groups[index].renders.push(reading.render.clone());
                set.groups[index].absorb(reading);
                continue;
            }
            seen.insert(reading.key(), set.groups.len());
            let id = set.allocate();
            set.groups
                .push(ContributionGroup::from_reading(id, reading));
        }
        set
    }

    /// One batch covering every pair, for flows that send no batch fields.
    fn covering(pairing: &PairingPlan) -> Self {
        let mut set = Self::default();
        if !pairing.groups.is_empty() {
            let id = set.allocate();
            set.groups.push(ContributionGroup {
                id,
                raws: pairing.groups.iter().map(|pair| pair.raw.clone()).collect(),
                renders: pairing
                    .groups
                    .iter()
                    .flat_map(|pair| &pair.jpegs)
                    .filter_map(|name| RenderFile::new(name))
                    .collect(),
                ..ContributionGroup::default()
            });
        }
        set
    }

    fn allocate(&mut self) -> BatchId {
        self.next += 1;
        BatchId(self.next)
    }

    pub(crate) fn groups(&self) -> &[ContributionGroup] {
        &self.groups
    }

    pub(crate) fn get(&self, id: BatchId) -> Option<&ContributionGroup> {
        self.groups.iter().find(|group| group.id == id)
    }

    pub(crate) fn batch_of(&self, render: &RenderFile) -> Option<BatchId> {
        self.groups
            .iter()
            .find(|group| group.renders.contains(render))
            .map(|group| group.id)
    }

    fn update(&mut self, id: BatchId, edit: impl FnOnce(&mut ContributionGroup)) {
        if let Some(group) = self.groups.iter_mut().find(|group| group.id == id) {
            edit(group);
        }
    }

    fn set_serial(&mut self, id: BatchId, opt_in: bool) {
        self.update(id, |group| group.serial_opt_in = opt_in);
    }

    /// Ends the upload so the next drop starts over, with serial sending off.
    fn finish(&mut self) {
        self.groups.clear();
        self.next = 0;
    }
}

impl ContributionGroup {
    fn from_reading(id: BatchId, reading: &PairReading) -> Self {
        let mut group = Self {
            id,
            raws: vec![reading.raw.clone()],
            renders: vec![reading.render.clone()],
            fields: GroupFields {
                body: reading.body.clone(),
                firmware: reading.firmware.clone(),
                sim: reading.sim.clone().unwrap_or_default(),
                controls: reading.controls_text(),
                recipe: false,
                target: String::new(),
                card: reading.card.is_some(),
                monochrome_opt_in: false,
                card_firmware_id: reading
                    .card
                    .as_ref()
                    .map(|tag| tag.firmware_id.as_str().to_owned())
                    .unwrap_or_default(),
                card_firmware_version: reading
                    .card
                    .as_ref()
                    .map(|tag| tag.firmware_version.as_str().to_owned())
                    .unwrap_or_default(),
            },
            serial_opt_in: false,
            malformed: reading.malformed,
            unavailable: Vec::new(),
            notes: Vec::new(),
            reproduced: reading.controls.clone(),
            unmapped: Vec::new(),
            family: reading.family.clone(),
            mono_filter: reading.mono_filter,
            monochrome: reading
                .monochrome
                .iter()
                .filter(|render| render.file == reading.render.as_str())
                .cloned()
                .collect(),
        };
        group.absorb(reading);
        group
    }

    fn absorb(&mut self, reading: &PairReading) {
        self.malformed |= reading.malformed;
        if self.family.is_none() {
            self.family = reading.family.clone();
        }
        if self.mono_filter.is_none() {
            self.mono_filter = reading.mono_filter;
        }
        for render in &reading.monochrome {
            if render.file == reading.render.as_str()
                && !self
                    .monochrome
                    .iter()
                    .any(|known| known.file == render.file)
            {
                self.monochrome.push(render.clone());
            }
        }
        for key in &reading.unmapped {
            if !self.unmapped.contains(key) {
                self.unmapped.push(key.clone());
            }
        }
        for reason in &reading.unavailable {
            if !self.unavailable.contains(reason) {
                self.unavailable.push(reason.clone());
            }
        }
        if let Some(note) = &reading.note {
            let note = format!("{}: {note}", reading.raw);
            if !self.notes.contains(&note) {
                self.notes.push(note);
            }
        }
    }

    /// The batch's own renders the schema classifies as monochrome.
    fn monochrome_renders(&self) -> usize {
        self.renders
            .iter()
            .filter(|render| {
                self.monochrome
                    .iter()
                    .any(|known| known.file == render.as_str())
            })
            .count()
    }

    fn is_monochrome(&self) -> bool {
        self.monochrome_renders() > 0
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CameraKey {
    make: Option<String>,
    model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct GroupKey {
    camera: Option<CameraKey>,
    firmware: Option<looklab_upload::FirmwareVersionString>,
    sim: Option<String>,
    render_class: looklab_upload::RenderClass,
    controls: Vec<(String, i64)>,
    card: Option<RenderTag>,
    malformed: bool,
    unresolved_render: Option<RenderFile>,
}

/// One JPEG's rendering settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PairReading {
    pub(crate) raw: String,
    pub(crate) render: RenderFile,
    pub(crate) camera: Option<CameraKey>,
    pub(crate) body: String,
    pub(crate) epoch: Option<u32>,
    pub(crate) firmware: String,
    pub(crate) sim: Option<String>,
    pub(crate) controls: Vec<(String, i64)>,
    pub(crate) unavailable: Vec<String>,
    pub(crate) unmapped: Vec<String>,
    pub(crate) card: Option<RenderTag>,
    pub(crate) malformed: bool,
    pub(crate) render_class: looklab_upload::RenderClass,
    pub(crate) family: Option<looklab_schema::LookId>,
    pub(crate) mono_filter: Option<looklab_schema::MonoFilter>,
    pub(crate) monochrome: Vec<looklab_wasm::MonochromeRender>,
    pub(crate) note: Option<String>,
}

impl PairReading {
    fn key(&self) -> GroupKey {
        GroupKey {
            camera: self.camera.clone(),
            firmware: looklab_upload::FirmwareVersionString::new(&self.firmware).ok(),
            sim: self.sim.clone(),
            render_class: self.render_class,
            controls: self.controls.clone(),
            card: self.card.clone(),
            malformed: self.malformed,
            unresolved_render: (self.epoch.is_none() || self.sim.is_none())
                .then(|| self.render.clone()),
        }
    }

    fn controls_text(&self) -> String {
        self.controls
            .iter()
            .map(|(key, step)| format!("{key}={step}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Identifies one prefill request, so a slower earlier answer cannot replace a newer one.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PrefillTicket {
    raw: String,
    epoch: u64,
    token: u64,
}

/// Tracks which prefill answer is current, per file and per drop.
#[derive(Debug, Default)]
pub(crate) struct PrefillLedger {
    epoch: u64,
    next: u64,
    latest: HashMap<String, (u64, u64)>,
}

impl PrefillLedger {
    /// Starts a new drop; every answer from an earlier one is stale.
    fn reset(&mut self) -> u64 {
        self.epoch += 1;
        self.next = 0;
        self.latest.clear();
        self.epoch
    }

    fn begin(&mut self, epoch: u64, raw: &str) -> PrefillTicket {
        if epoch != self.epoch {
            return PrefillTicket {
                raw: raw.to_owned(),
                epoch,
                token: 0,
            };
        }
        self.next += 1;
        self.latest.insert(raw.to_owned(), (epoch, self.next));
        PrefillTicket {
            raw: raw.to_owned(),
            epoch,
            token: self.next,
        }
    }

    fn accept(&self, ticket: &PrefillTicket) -> bool {
        self.epoch == ticket.epoch
            && ticket.token != 0
            && self.latest.get(&ticket.raw) == Some(&(ticket.epoch, ticket.token))
    }

    fn is_current(&self, epoch: u64) -> bool {
        self.epoch == epoch
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct ContributionDraft {
    #[serde(default)]
    files: Vec<DraftFile>,
    #[serde(default)]
    batches: Vec<ContributionGroup>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PreparedDraft {
    groups: Vec<DraftPreparedGroup>,
    seeds: Vec<SeedRow>,
    failures: Vec<FailureRow>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DraftPreparedGroup {
    raw: String,
    renders: Vec<RenderFile>,
    images: Vec<PreparedImageView>,
    bytes_in: u64,
    millis: f64,
    memory: u64,
}

struct MenuSetting {
    key: looklab_schema::ControlKey,
    step: i64,
}

fn menu_settings(text: &str) -> Vec<MenuSetting> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some(MenuSetting {
                key: looklab_schema::ControlKey::new(key.trim()).ok()?,
                step: value.trim().parse().ok()?,
            })
        })
        .collect()
}

fn control_step(text: &str, key: &looklab_schema::ControlKey) -> Option<i64> {
    menu_settings(text)
        .into_iter()
        .find(|setting| setting.key == *key)
        .map(|setting| setting.step)
}

fn set_control_step(
    batches: RwSignal<BatchSet>,
    id: BatchId,
    key: &looklab_schema::ControlKey,
    step: i64,
) {
    batches.update(|set| {
        set.update(id, |group| {
            let mut fields = menu_settings(&group.fields.controls);
            match fields.iter_mut().find(|field| field.key == *key) {
                Some(field) => field.step = step,
                None => fields.push(MenuSetting {
                    key: key.clone(),
                    step,
                }),
            }
            group.fields.controls = fields
                .into_iter()
                .map(|field| format!("{}={}", field.key, field.step))
                .collect::<Vec<_>>()
                .join("\n");
        });
    });
}

#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
export async function contributePlatformStore() {
    return !!globalThis.PublicKeyCredential && await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable();
}
export function contributeChoosePhotos(button, input) {
    button.closest("dialog").close();
    input.click();
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = contributePlatformStore)]
    async fn platform_store() -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = contributeChoosePhotos)]
    fn choose_photos(
        button: &wasm_bindgen::JsValue,
        input: &wasm_bindgen::JsValue,
    ) -> Result<(), wasm_bindgen::JsValue>;
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessedGroup {
    pub(crate) raw: String,
    pub(crate) renders: Vec<RenderFile>,
    pub(crate) batch: BatchId,
    pub(crate) images: Vec<PrepSummary>,
    pub(crate) blobs: Vec<Vec<u8>>,
    pub(crate) bytes_in: u64,
    pub(crate) millis: f64,
    pub(crate) memory: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SeedRow {
    pub(crate) name: String,
    pub(crate) seed: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FailureRow {
    pub(crate) name: String,
    pub(crate) message: String,
    pub(crate) donated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Drop,
    Confirm,
    Processing,
    Inspector,
    Send,
    Done,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SentSummary {
    pub(crate) renders: usize,
    pub(crate) deferred: usize,
    pub(crate) cameras: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpWay {
    ExistingPhotos,
    WantedShots,
    Rerender,
    PhonePattern,
    Gradient,
    ColorChart,
}

struct HelpGuide {
    title: &'static str,
    time: &'static str,
    impact: Impact,
    description: &'static str,
    steps: &'static [&'static str],
    action: &'static str,
}

impl HelpWay {
    const ALL: [Self; 6] = [
        Self::ExistingPhotos,
        Self::WantedShots,
        Self::Rerender,
        Self::PhonePattern,
        Self::Gradient,
        Self::ColorChart,
    ];

    fn guide(self) -> HelpGuide {
        match self {
            Self::ExistingPhotos => HelpGuide {
                title: "Share photos you already have",
                time: "About a minute",
                impact: Impact::Medium,
                description: "Start with RAW+JPEG pairs from your library.",
                steps: &[
                    "Find matching RAW and JPEG files straight from your camera, shot with a built-in look. Use the original JPEG, not an edited or exported copy.",
                    "Look in your memory card's DCIM folder or the folder where you keep camera originals. If you use a photo library, export the unmodified originals.",
                    "Choose both files in each pair, or add their folder. We pair them and read their camera settings on this device.",
                    "Check the groups, then review the color samples. Nothing is sent until you confirm the upload.",
                ],
                action: "Choose photos",
            },
            Self::WantedShots => HelpGuide {
                title: "Take a few wanted shots",
                time: "5 to 10 minutes",
                impact: Impact::High,
                description: "Fill a gap with colors we need to measure.",
                steps: &[
                    "Choose a request for your camera and the built-in look you will use.",
                    "Shoot RAW+JPEG at the lowest native ISO. Turn off lens corrections where possible and avoid motion.",
                    "Photograph the requested colors and lighting. Keep the original RAW+JPEG pairs to add below.",
                ],
                action: "I've got them, add photos",
            },
            Self::Rerender => HelpGuide {
                title: "Re-render a few RAWs in camera",
                time: "About 2 minutes",
                impact: Impact::High,
                description: "Try the same photos with other built-in looks.",
                steps: &[
                    "Choose 3 RAW photos with a mix of colors, shadows and highlights. Keep the original RAW files.",
                    "Use your camera's playback menu to open RAW conversion. Select a built-in look and save a new JPEG.",
                    "Repeat with other built-in looks for the same 3 RAWs. Save each JPEG without overwriting the earlier versions.",
                    "Add the original RAWs and all the re-rendered JPEGs together. We group the versions on this device.",
                ],
                action: "Add the re-rendered JPEGs",
            },
            Self::PhonePattern => HelpGuide {
                title: "Photograph a pattern on your phone",
                time: "About 3 minutes",
                impact: Impact::Medium,
                description: "Add a wider range of saturated colors.",
                steps: &[
                    "Open the measurement pattern on your phone",
                    "Turn off Night Shift, True Tone and adaptive color on the phone.",
                    "Fill about 60% of the camera frame with the pattern and defocus slightly.",
                    "Shoot RAW+JPEG at your lowest native ISO, with lens corrections off where possible. Take 3 shots at 0, −1 and −2 EV exposure compensation.",
                ],
                action: "Add the pattern shots",
            },
            Self::Gradient => HelpGuide {
                title: "Photograph a gradient",
                time: "About 2 minutes",
                impact: Impact::Medium,
                description: "Help measure contrast and neutral tint.",
                steps: &[
                    "Find a smooth gradient from white to black, lit by a single light source.",
                    "Set white balance for that light. Shoot RAW+JPEG at the lowest native ISO, with lens corrections off where possible.",
                    "Defocus the gradient so the change from light to dark is smooth and covers the frame evenly.",
                    "Take one shot with the white end bright enough to clip in the RAW. Keep both the original RAW and JPEG.",
                ],
                action: "Add the gradient shot",
            },
            Self::ColorChart => HelpGuide {
                title: "Photograph a color chart",
                time: "About 5 minutes",
                impact: Impact::High,
                description: "Help measure how your camera sees color.",
                steps: &[
                    "Use a ColorChecker Classic 24, SG 140 or Passport chart.",
                    "Light the chart evenly without glare. Keep it still and fill 30 to 60% of the frame.",
                    "Shoot RAW+JPEG at the lowest native ISO. Turn off lens corrections where possible.",
                    "Take 3 shots at 0, −1 and −2 EV exposure compensation. Keep the original RAW+JPEG pairs.",
                ],
                action: "Add the chart shots",
            },
        }
    }
}

#[component]
fn HelpWalkthrough(
    way: HelpWay,
    open: RwSignal<bool>,
    on_choose: Callback<leptos::ev::MouseEvent>,
) -> impl IntoView {
    let guide = way.guide();
    view! {
        <Dialog title=guide.title.to_owned() open>
            <div class="help-walkthrough">
                <p class="help-meta">{guide.time} " · " {guide.impact.label()}</p>
                <ol class="help-steps">
                    {guide.steps.iter().enumerate().map(|(index, step)| {
                        if way == HelpWay::PhonePattern && index == 0 {
                            view! { <li><PhonePatternLink label=*step/></li> }.into_any()
                        } else {
                            view! { <li>{*step}</li> }.into_any()
                        }
                    }).collect_view()}
                </ol>
                {(way == HelpWay::WantedShots).then(|| view! {
                    <WantedCards limit=3 show_actions=false/>
                    <p>
                        <a href=crate::base::site_url("/contribute/wanted")>
                            "See all wanted shots"
                        </a>
                    </p>
                })}
                <button class="button-primary" on:click=move |event| on_choose.run(event)>
                    {guide.action}
                </button>
            </div>
        </Dialog>
    }
}

#[component]
fn PhonePatternLink(label: &'static str) -> impl IntoView {
    let path = crate::base::site_url(&format!(
        "/pattern/{}",
        super::pattern::fixture_seed().to_hex()
    ));
    let absolute = web_sys::window()
        .and_then(|window| window.location().href().ok())
        .and_then(|base| web_sys::Url::new_with_base(&path, &base).ok());
    view! {
        <a href=path target="_blank" rel="noopener noreferrer">{label}</a>
        <p>"Tap the pattern to hide browser toolbars where supported. Set the screen brightness to maximum."</p>
        {match absolute {
            Some(url) => {
                let address = url.href();
                let qr = qrcodegen::QrCode::encode_text(
                    &address,
                    qrcodegen::QrCodeEcc::Medium,
                );
                view! {
                    <div class="phone-pattern-target">
                        <figure>
                            {match qr {
                                Ok(code) => pattern_qr(code).into_any(),
                                Err(_) => view! {
                                    <p role="alert">"This address is too long for a QR code. Open or copy the link instead."</p>
                                }.into_any(),
                            }}
                            <figcaption>"Scan with your phone"</figcaption>
                        </figure>
                        <label>
                            "Or copy this link"
                            <input type="text" readonly aria-label="Pattern URL" prop:value=address/>
                        </label>
                    </div>
                }.into_any()
            },
            None => view! {
                <p role="alert">"The phone link is unavailable in this browser."</p>
            }.into_any(),
        }}
    }
}

fn pattern_qr(code: qrcodegen::QrCode) -> impl IntoView {
    let side = code.size();
    let extent = side + 8;
    let mut modules = String::with_capacity((side * side * 10) as usize);
    for y in 0..side {
        let mut x = 0;
        while x < side {
            if !code.get_module(x, y) {
                x += 1;
                continue;
            }
            let start = x;
            while x < side && code.get_module(x, y) {
                x += 1;
            }
            let length = x - start;
            let _ = write!(modules, "M{} {}h{length}v1h-{length}z", start + 4, y + 4);
        }
    }
    view! {
        <svg
            class="pattern-qr"
            xmlns="http://www.w3.org/2000/svg"
            viewBox=format!("0 0 {extent} {extent}")
            role="img"
            aria-label="Scan to open the measurement pattern"
            shape-rendering="crispEdges"
        >
            <rect width=extent height=extent fill="#ffffff"/>
            <path d=modules fill="#000000"/>
        </svg>
    }
}

#[component]
pub fn ContributePage() -> impl IntoView {
    let session_context = crate::session::use_session();
    let stage = RwSignal::new(Stage::Drop);
    let current = RwSignal::new(StepIndex(0));
    let furthest = RwSignal::new(StepIndex(0));
    let file_picker = NodeRef::<leptos::html::Input>::new();
    let selected_help = RwSignal::new(HelpWay::ExistingPhotos);
    let help_open = RwSignal::new(false);
    Effect::new(move |_| {
        stage.set(match current.get().0 {
            0 => Stage::Drop,
            1 => Stage::Confirm,
            2 => Stage::Processing,
            3 => Stage::Inspector,
            4 => Stage::Send,
            _ => Stage::Done,
        });
    });
    let plan = RwSignal::new(None::<PairingPlan>);
    let all_files = RwSignal::new_local(Vec::<LocalFile>::new());
    let parsed_metadata = StoredValue::new_local(Vec::<(String, ParsedMetadata)>::new());
    let batches = RwSignal::new(BatchSet::default());
    let batch_epoch = RwSignal::new(0_u64);
    let selected_batch = RwSignal::new(None::<BatchId>);
    let catalog = RwSignal::new(None::<GroupCatalog>);
    let snapshot_version = RwSignal::new(0_u64);
    let prefill_ledger = StoredValue::new_local(PrefillLedger::default());
    let worker_count = RwSignal::new(default_worker_count());
    let processed = RwSignal::new(Vec::<ProcessedGroup>::new());
    let completed = RwSignal::new(Vec::<BatchId>::new());
    let seeds = RwSignal::new(Vec::<SeedRow>::new());
    let coverage = RwSignal::new(None::<CoverageMap>);
    let points = RwSignal::new(Vec::<[f32; 3]>::new());
    let failures = RwSignal::new(Vec::<FailureRow>::new());
    let send_parts = RwSignal::new(None::<(usize, usize)>);
    let sent_summary = RwSignal::new(None::<SentSummary>);
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let invalidate_preparation = Callback::new(move |()| {
        processed.set(Vec::new());
        seeds.set(Vec::new());
        failures.set(Vec::new());
        coverage.set(None);
        points.set(Vec::new());
        furthest.set(StepIndex(1));
        current.set(StepIndex(1));
        message.set(Some(
            "Your B&W selection changed. Process these photos again before sending.".to_owned(),
        ));
    });
    let on_help_choose = Callback::new(move |event: leptos::ev::MouseEvent| {
        help_open.set(false);
        current.set(StepIndex(0));
        if let (Some(button), Some(input)) = (event.current_target(), file_picker.get())
            && choose_photos(button.as_ref(), input.as_ref()).is_err()
        {
            message.set(Some(
                "Choose files in the drop area to continue.".to_owned(),
            ));
        }
    });
    let status_links = RwSignal::new(Vec::<String>::new());
    let available = crate::base::server_available();
    let no_platform_store = RwSignal::new(false);
    let draft_loaded = RwSignal::new(false);
    let draft_complete = RwSignal::new(false);
    let draft_unavailable = RwSignal::new(false);
    let draft_files = RwSignal::new(Vec::<DraftFile>::new());
    let draft_store = StoredValue::new_local(ContributionDraftStore::new());
    let replace_batches = move |readings: Vec<PairReading>| {
        let set = BatchSet::from_readings(&readings);
        let first = set.groups().first().map(|group| group.id);
        batches.set(set);
        selected_batch.set(first);
        batch_epoch.update(|epoch| *epoch += 1);
    };
    on_cleanup(move || draft_store.with_value(ContributionDraftStore::flush));
    spawn_local(async move {
        let _ = idb::remove(idb::StorageKey::DRAFT).await;
        let stored = wasm_bindgen_futures::JsFuture::from(
            draft_store.with_value(ContributionDraftStore::settings),
        )
        .await;
        if let Ok(value) = stored
            && let Some(text) = value.as_string()
            && let Ok(draft) = serde_json::from_str::<ContributionDraft>(&text)
        {
            let mut files = Vec::with_capacity(draft.files.len());
            for file in &draft.files {
                let pending = draft_store.with_value(|store| store.stored_file(file));
                match read_draft_file(pending, file).await {
                    Ok(file) => files.push(file),
                    Err(error) => {
                        message.set(Some(error));
                        draft_loaded.set(true);
                        return;
                    }
                }
            }
            let scanned = match scan_metadata(&files).await {
                Ok(scanned) => scanned,
                Err(error) => {
                    message.set(Some(error));
                    draft_loaded.set(true);
                    return;
                }
            };
            let pairing = inspect_metadata(&scanned);
            let parsed = scanned
                .into_iter()
                .filter_map(|file| file.into_parsed())
                .collect::<Vec<(String, ParsedMetadata)>>();
            if let Ok(pairing) = pairing
                && !pairing.groups.is_empty()
            {
                let set = if draft.batches.is_empty()
                    || draft.batches.iter().any(|group| group.renders.is_empty())
                {
                    let Some(epoch) = prefill_ledger.try_update_value(PrefillLedger::reset) else {
                        draft_loaded.set(true);
                        return;
                    };
                    let readings =
                        read_pairs(&pairing, &files, &parsed, prefill_ledger, epoch).await;
                    BatchSet::from_readings(&readings)
                } else {
                    BatchSet::from_groups(draft.batches)
                };
                let routing = set.clone();
                let first = set.groups().first().map(|group| group.id);
                batches.set(set);
                selected_batch.set(first);
                batch_epoch.update(|epoch| *epoch += 1);
                all_files.set(files);
                parsed_metadata.set_value(parsed);
                draft_files.set(draft.files);
                plan.set(Some(pairing));
                let prepared = wasm_bindgen_futures::JsFuture::from(
                    draft_store.with_value(ContributionDraftStore::samples),
                )
                .await
                .ok()
                .and_then(|value| value.as_string())
                .and_then(|text| serde_json::from_str::<PreparedDraft>(&text).ok());
                let PreparedDraft {
                    groups,
                    seeds: restored_seeds,
                    failures: restored_failures,
                } = prepared.unwrap_or(PreparedDraft {
                    groups: Vec::new(),
                    seeds: Vec::new(),
                    failures: Vec::new(),
                });
                let groups = groups
                    .into_iter()
                    .filter_map(|group| {
                        let batch = routing.batch_of(group.renders.first()?)?;
                        if group.renders.len() != group.images.len()
                            || group
                                .renders
                                .iter()
                                .any(|render| routing.batch_of(render) != Some(batch))
                        {
                            return None;
                        }
                        Some(ProcessedGroup {
                            batch,
                            raw: group.raw,
                            renders: group.renders,
                            images: looklab_wasm::prep::summarize(&group.images),
                            blobs: group.images.into_iter().map(|image| image.blob).collect(),
                            bytes_in: group.bytes_in,
                            millis: group.millis,
                            memory: group.memory,
                        })
                    })
                    .collect::<Vec<_>>();
                if !groups.is_empty() {
                    let blobs = groups
                        .iter()
                        .flat_map(|group| group.blobs.iter().cloned())
                        .collect::<Vec<_>>();
                    coverage.set(coverage_map(&blobs).ok());
                    points.set(sample_points(&blobs, usize::MAX).unwrap_or_default());
                    processed.set(groups);
                    seeds.set(restored_seeds);
                    failures.set(restored_failures);
                    current.set(StepIndex(3));
                }
                if processed.with(Vec::is_empty) {
                    current.set(StepIndex(1));
                }
                message.set(Some(
                    "Your local draft is ready to resume. Nothing has been sent.".to_owned(),
                ));
            }
        }
        draft_loaded.set(true);
    });
    Effect::new(move |_| {
        if !draft_loaded.get() {
            return;
        }
        if draft_complete.get() {
            return;
        }
        if busy.get() {
            return;
        }
        let draft = ContributionDraft {
            files: draft_files.get(),
            batches: batches.with(|set| set.groups().to_vec()),
        };
        let retain_prepared = processed.with(|groups| !groups.is_empty());
        if let Ok(text) = serde_json::to_string(&draft) {
            draft_store.with_value(|store| store.schedule(&text, retain_prepared));
        }
    });
    spawn_local(async move {
        no_platform_store.set(
            platform_store()
                .await
                .ok()
                .and_then(|value| value.as_bool())
                == Some(false),
        );
    });
    Effect::new(move |_| {
        let Some(facts) = plan.with(|plan| plan.as_ref().map(|plan| plan.facts.clone())) else {
            return;
        };
        spawn_local(async move {
            if let Ok(index) = data::index().await {
                snapshot_version.set(index.community_snapshot_version);
                let mut schemas = Vec::new();
                for camera in &index.bodies {
                    if facts.iter().any(|fact| {
                        matching_camera(fact, &index).is_some_and(|matched| matched.id == camera.id)
                    }) && let Ok(page) = data::body(camera.id.make(), camera.id.model()).await
                        && let Ok(schema) = looklab_schema::validate_control_schema(&page.schema)
                    {
                        schemas.push(schema);
                    }
                }
                catalog.set(Some(GroupCatalog { index, schemas }));
            }
        });
    });

    let on_files = Callback::new(move |selected: Vec<web_sys::File>| {
        busy.set(true);
        spawn_local(async move {
            let files = selected_files(selected);
            let scanned = match scan_metadata(&files).await {
                Ok(scanned) => scanned,
                Err(error) => {
                    message.set(Some(error));
                    busy.set(false);
                    return;
                }
            };
            let pairing = inspect_metadata(&scanned);
            let parsed = scanned
                .into_iter()
                .filter_map(|file| file.into_parsed())
                .collect::<Vec<(String, ParsedMetadata)>>();
            match pairing {
                Ok(pairing) => {
                    processed.set(Vec::new());
                    seeds.set(Vec::new());
                    failures.set(Vec::new());
                    coverage.set(None);
                    points.set(Vec::new());
                    furthest.set(StepIndex(1));
                    draft_complete.set(false);
                    status_links.set(Vec::new());
                    match draft_file_entries(&files) {
                        Ok((references, entries)) => {
                            let pending = draft_store
                                .with_value(|store| store.replace_files(entries.as_ref()));
                            if wasm_bindgen_futures::JsFuture::from(pending).await.is_err() {
                                draft_unavailable.set(true);
                            }
                            draft_files.set(references);
                        }
                        Err(error) => message.set(Some(error)),
                    }
                    current.set(StepIndex(1));
                    plan.set(Some(pairing.clone()));
                    let Some(epoch) = prefill_ledger.try_update_value(PrefillLedger::reset) else {
                        busy.set(false);
                        return;
                    };
                    let readings =
                        read_pairs(&pairing, &files, &parsed, prefill_ledger, epoch).await;
                    all_files.set(files);
                    parsed_metadata.set_value(parsed);
                    if prefill_ledger.with_value(|ledger| ledger.is_current(epoch)) {
                        replace_batches(readings);
                    }
                }
                Err(error) => message.set(Some(error.to_string())),
            }
            busy.set(false);
        });
    });

    let process = move |_| {
        let Some(pairing) = plan.get() else {
            return;
        };
        let files = all_files.get();
        let batches = batches.get_untracked();
        let count = worker_count.get().max(1);
        busy.set(true);
        processed.set(Vec::new());
        furthest.set(StepIndex(2));
        current.set(StepIndex(2));
        message.set(Some(
            "Processing locally. Nothing leaves this page yet.".to_owned(),
        ));
        spawn_local(async move {
            let validation = validate_batch_sims(&batches).await;
            if let Err(error) = validation {
                message.set(Some(error));
                current.set(StepIndex(1));
                busy.set(false);
                return;
            }
            let result = run_files_progress(
                &pairing,
                &batches,
                &files,
                count,
                Some(Callback::new(move |group: ProcessedGroup| {
                    processed.update(|groups| groups.push(group));
                    processed.with(|groups| {
                        let blobs = groups
                            .iter()
                            .flat_map(|group| group.blobs.iter().cloned())
                            .collect::<Vec<_>>();
                        coverage.set(coverage_map(&blobs).ok());
                    });
                })),
            )
            .await;
            match result {
                Ok((groups, seed_rows, failure_rows)) => {
                    let blobs: Vec<Vec<u8>> = groups
                        .iter()
                        .flat_map(|group| group.blobs.iter().cloned())
                        .collect();
                    coverage.set(coverage_map(&blobs).ok());
                    points.set(sample_points(&blobs, usize::MAX).unwrap_or_default());
                    let prepared = PreparedDraft {
                        groups: groups
                            .iter()
                            .map(|group| DraftPreparedGroup {
                                raw: group.raw.clone(),
                                renders: group.renders.clone(),
                                images: prepared_images(std::slice::from_ref(group)),
                                bytes_in: group.bytes_in,
                                millis: group.millis,
                                memory: group.memory,
                            })
                            .collect(),
                        seeds: seed_rows.clone(),
                        failures: failure_rows.clone(),
                    };
                    if let Ok(text) = serde_json::to_string(&prepared) {
                        let pending = draft_store.with_value(|store| store.prepared(&text));
                        if wasm_bindgen_futures::JsFuture::from(pending).await.is_err() {
                            draft_unavailable.set(true);
                        }
                    }
                    processed.set(groups);
                    seeds.set(seed_rows);
                    failures.set(failure_rows);
                    if processed.with(Vec::is_empty) {
                        current.set(StepIndex(1));
                        message.set(Some("No photos could be processed. Check the excluded files below and try an original RAW+JPEG pair.".to_owned()));
                    } else {
                        current.set(StepIndex(3));
                        message.set(None);
                    }
                }
                Err(error) => {
                    message.set(Some(error));
                    current.set(StepIndex(1));
                }
            }
            busy.set(false);
        });
    };

    let upload = move |_| {
        let processed_groups = processed.get();
        let set = batches.get_untracked();
        if processed_groups.is_empty()
            || set.groups().is_empty()
            || busy.get()
            || draft_complete.get()
        {
            return;
        }
        let mut pending = Vec::new();
        let version = snapshot_version.get_untracked();
        for group in set.groups() {
            if completed.with(|done| done.contains(&group.id)) {
                continue;
            }
            if group.is_monochrome() && !group.fields.monochrome_opt_in {
                continue;
            }
            let images = prepared_images_of(&processed_groups, group.id);
            if images.is_empty() {
                continue;
            }
            let settings = match group_settings_for(group, &images, version) {
                Ok(settings) => settings,
                Err(error) => {
                    message.set(Some(format!("Batch {}: {error}", group.id.0)));
                    return;
                }
            };
            let seed_rows = seeds
                .get()
                .into_iter()
                .filter(|row| group.raws.iter().any(|raw| raw == &row.name))
                .collect::<Vec<_>>();
            let serial = if group.serial_opt_in {
                seed_rows.first().and_then(|seed| {
                    parsed_metadata.with_value(|files| {
                        files
                            .iter()
                            .find(|(name, _)| name == &seed.name)
                            .and_then(|(_, metadata)| serial_ascii_from_metadata(metadata))
                    })
                })
            } else {
                None
            };
            pending.push(PendingBatch {
                id: group.id,
                settings,
                images,
                serial,
            });
        }
        if pending.is_empty() {
            message.set(Some("Nothing is left to send.".to_owned()));
            return;
        }
        let photos = pending.iter().map(|batch| batch.images.len()).sum();
        let sent = sent_summary_of(&pending);
        busy.set(true);
        spawn_local(async move {
            if let Err(error) = validate_batch_sims(&set).await {
                message.set(Some(error));
                busy.set(false);
                return;
            }
            send_parts.set(Some((0, photos)));
            message.set(Some(
                "Confirm once on your device. Preparing your photos (this keeps spam out)."
                    .to_owned(),
            ));
            let outcome = upload_send(session_context, pending, send_parts).await;
            for (id, reply) in outcome.accepted {
                if let Some(token) = reply.status_token.as_ref()
                    && let Ok(token) = webauthn::StatusToken::try_from(token.clone())
                {
                    let context = session_context;
                    if let crate::session::SessionState::SignedIn(mut profile) =
                        context.state.get_untracked()
                    {
                        profile.status_token = token;
                        if context.set_signed_in(profile.clone()).await.is_err() {
                            context
                                .state
                                .set(crate::session::SessionState::SignedIn(profile));
                        }
                    }
                }
                if let Some(token) = reply.status_token.as_ref() {
                    let link = crate::base::site_url(&format!("/status/{token}"));
                    status_links.update(|links| links.push(link));
                }
                completed.update(|done| done.push(id));
            }
            if let Some((_, error)) = outcome.error {
                message.set(Some(format!("Your photos could not all be sent: {error}")));
                busy.set(false);
                return;
            }
            sent_summary.set(Some(sent));
            message.set(None);
            draft_complete.set(true);
            let pending = draft_store.with_value(ContributionDraftStore::clear);
            let _ = wasm_bindgen_futures::JsFuture::from(pending).await;
            batches.update(BatchSet::finish);
            completed.set(Vec::new());
            processed.set(Vec::new());
            seeds.set(Vec::new());
            let excluded = failures.get_untracked();
            all_files.update(|files| {
                files.retain(|file| excluded.iter().any(|row| row.name == file.name()));
            });
            parsed_metadata.set_value(Vec::new());
            points.set(Vec::new());
            current.set(StepIndex(5));
            busy.set(false);
        });
    };

    let download_payload = move |_| {
        let groups = processed.get();
        let set = batches.get_untracked();
        let payload = serde_json::json!({
            "format": "looklab-payload-view/1",
            "authoritative": false,
            "note": "A convenience view, not a trust anchor. Verify the request body actually sent against the HAR.",
            "batches": set.groups().iter().map(|group| serde_json::json!({
                "body": group.fields.body,
                "firmware": group.fields.firmware,
                "sim": group.fields.sim,
                "controls": group.fields.controls,
                "raws": group.raws,
                "serial": group.serial_opt_in,
            })).collect::<Vec<_>>(),
            "images": groups
                .iter()
                .flat_map(|group| group.images.iter())
                .map(|image| serde_json::json!({
                    "metadata": image.metadata,
                    "removed": image.removed,
                    "blob_bytes": image.blob_len,
                    "rerender_group": image.rerender_group,
                }))
                .collect::<Vec<_>>(),
        });
        let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
        if let Err(error) = download("looklab-payload.json", text.as_bytes(), "application/json") {
            message.set(Some(error));
        }
    };

    let download_seeds = move |_| {
        let entries = seeds
            .get()
            .into_iter()
            .filter_map(|row| LocalImageKey::new(&row.name).map(|key| (key, row.seed)))
            .collect::<Vec<_>>();
        match looklab_wasm::build_seed_file(PRIOR_VERSION, &entries) {
            Ok(json) => {
                if let Err(error) =
                    download("looklab-seeds.json", json.as_bytes(), "application/json")
                {
                    message.set(Some(error));
                }
            }
            Err(error) => message.set(Some(error.to_string())),
        }
    };

    let donation = move |name: String| {
        let Some(file) = all_files.with(|files| {
            files
                .iter()
                .find(|file| file.name() == name.as_str())
                .cloned()
        }) else {
            return;
        };
        spawn_local(async move {
            let bytes = match file.bytes().await {
                Ok(bytes) => bytes,
                Err(error) => {
                    message.set(Some(error));
                    return;
                }
            };
            if let Err(error) = download(file.name(), &bytes, "application/octet-stream") {
                message.set(Some(error));
                return;
            }
            let mut rows = failures.get();
            if let Some(row) = rows.iter_mut().find(|row| row.name == file.name()) {
                row.donated = true;
            }
            failures.set(rows);
            let consent = format!(
                "{{\"file\":\"{}\",\"consent\":\"fixture donation\",\"recorded_at\":{}}}",
                file.name(),
                js_sys::Date::now()
            );
            let _ = idb::put(idb::StorageKey::FIXTURE_CONSENT, &consent).await;
            message.set(Some(
                "Consent saved on this device. Follow the fixture donation instructions when sharing the downloaded file."
                    .to_owned(),
            ));
        });
    };

    let on_donate = Callback::new(donation);

    view! {
        <div class="contribute-page">
        <header class="contribute-intro" class:contribution-active=move || stage.get() != Stage::Drop>
            <p class="eyebrow">"Made together, kept private"</p>
            <h1>"Help build looks"</h1>
            <p class="lede">"Your photos never leave this device. We extract color samples, and you see exactly what's sent before it goes."</p>
            <p>"Shared color samples help measure the looks photographers love. You need RAW+JPEG pairs from a supported camera, shot with a built-in look."</p>
        </header>
        <p class="notice" hidden=move || !no_platform_store.get()>"This computer can't store passkeys. Your browser will offer to use your phone (scan a QR code) or a security key."</p>
        {(!available).then(crate::ui::preview_notice)}
        <section class="ways-to-help" hidden=move || stage.get() != Stage::Drop>
            <h2>"Ways to help"</h2>
            <div class="help-grid">
                {HelpWay::ALL.into_iter().map(|way| {
                    let guide = way.guide();
                    view! {
                        <button
                            type="button"
                            class="help-card help-card-button"
                            aria-label=guide.title
                            aria-haspopup="dialog"
                            disabled=move || busy.get() || !draft_loaded.get()
                            on:click=move |_| {
                                selected_help.set(way);
                                help_open.set(true);
                            }
                        >
                            <span class="help-meta">{guide.time} " · " {guide.impact.label()}</span>
                            <span class="help-title">{guide.title}</span>
                            <span class="help-description">{guide.description}</span>
                        </button>
                    }
                }).collect_view()}
            </div>
        </section>
        {move || view! {
            <HelpWalkthrough
                way=selected_help.get()
                open=help_open
                on_choose=on_help_choose
            />
        }}
        <Stepper
            current
            furthest
            steps=["Add photos", "Check groups", "Process", "Review", "Send", "Done"]
                .into_iter()
                .map(|title| WizardStep { title: title.to_owned(), summary: None })
                .collect()
            summaries=Signal::derive(move || {
                let pairs = plan.with(|plan| plan.as_ref().map(|plan| plan.groups.len()).unwrap_or(0));
                let samples = points.with(Vec::len);
                vec![
                    Some(format!("{pairs} {}", if pairs == 1 { "pair" } else { "pairs" })),
                    Some(format!("{pairs} {}", if pairs == 1 { "group" } else { "groups" })),
                    coverage.with(|map| map.as_ref().map(|map| format!("{} {} processed", map.images, if map.images == 1 { "photo" } else { "photos" }))),
                    (samples > 0).then(|| format!("{samples} color samples")),
                    sent_summary.get().map(|sent| format!("{} {}", sent.renders, if sent.renders == 1 { "photo" } else { "photos" })),
                    None,
                ]
            })
            locked=Signal::derive(move || busy.get() || draft_complete.get())
            finished=Signal::derive(move || draft_complete.get())
            can_advance=Signal::derive(move || !busy.get() && matches!(stage.get(), Stage::Inspector))
        />
        {move || (stage.get() != Stage::Send).then(|| message.get()).flatten().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        {move || draft_unavailable.get().then(|| view! {
            <p class="notice">"This browser couldn't keep a local draft, so these photos won't be restored after a reload. Processing and sending still work."</p>
        })}

        <section class="step" hidden=move || stage.get() != Stage::Drop>
            <h2>"Drop files or a folder"</h2>
            <DropZone
                on_files
                file_picker
                accept=crate::files::camera_originals_accept_attribute()
                disabled=Signal::derive(move || busy.get() || !draft_loaded.get())
                prompt="Drop RAW+JPEG files or a folder here. Photos stay on this device, even when you send color samples.".to_owned()
            />
            <p class="muted">"Your files, settings and prepared samples stay in a local draft in this browser. Keep your originals in case browser storage is cleared."</p>
        </section>

        <section class="step" hidden=move || stage.get() != Stage::Confirm>
            <h2>"Check the groups"</h2>
            {move || plan.get().map(|pairing| view! {
                <Pairing plan=pairing batches=batches catalog=catalog/>
            })}
            <p>"Each group of photos that share a camera, built-in look and settings is sent as one batch. Check every batch below. If a batch uses a recipe to match another look, choose that option and name the target."</p>
            <h3>"Batches"</h3>
            <ul class="batch-list">
                {move || {
                    let _ = batch_epoch.get();
                    let set = batches.get_untracked();
                    set.groups().iter().map(|group| {
                        let id = group.id;
                        let title = move || {
                            batches.with(|set| {
                                set.get(id).map_or_else(String::new, |group| {
                                    format!("Batch {}: {} photos, {}", id.0, group.raws.len(), if group.fields.sim.is_empty() { "no look set".to_owned() } else { title_case_words(&group.fields.sim) })
                                })
                            })
                        };
                        view! {
                            <li>
                                <button type="button" class="batch-choice"
                                    class:selected=move || selected_batch.get() == Some(id)
                                    aria-pressed=move || if selected_batch.get() == Some(id) { "true" } else { "false" }
                                    on:click=move |_| selected_batch.set(Some(id))
                                >
                                    <span class="batch-choice-title">{title}</span>
                                    <span class="batch-choice-meta">{move || batches.with(|set| {
                                        set.get(id).map(|group| group.raws.join(", ")).unwrap_or_default()
                                    })}</span>
                                </button>
                            </li>
                        }
                    }).collect_view()
                }}
            </ul>
            {move || {
                let _ = batch_epoch.get();
                let set = batches.get_untracked();
                let Some(id) = selected_batch.get() else {
                    return view! { <p class="muted">"No batches yet. Add photos to build them."</p> }.into_any();
                };
                if set.get(id).is_none() {
                    return view! { <p class="muted">"No batches yet. Add photos to build them."</p> }.into_any();
                }
                let first = set.groups().first().map(|group| group.id) == Some(id);
                let firmware_ids = plan.with(|plan| {
                    let mut ids = Vec::new();
                    if let Some(plan) = plan {
                        for fact in &plan.facts {
                            if let Some(tag) = &fact.render_tag {
                                let id = tag.firmware_id.as_str().to_owned();
                                if !ids.contains(&id) {
                                    ids.push(id);
                                }
                            }
                        }
                    }
                    ids
                });
                view! {
                    <BatchEditor id=id first=first batches=batches firmware_ids=firmware_ids on_preparation_change=invalidate_preparation/>
                }.into_any()
            }}
            {move || {
                let rows = failures.get();
                (!rows.is_empty()).then(|| view! {
                    <section class="excluded" aria-label="Excluded files">
                        <h3>"Files left out"</h3>
                        {excluded_files(rows, on_donate)}
                    </section>
                })
            }}
            <Disclosure summary="Processing options".to_owned()>
            <label for="workers">"Parallel processing tasks"</label>
            <select id="workers" on:change=move |event| {
                worker_count.set(event_target_value(&event).parse().unwrap_or(1))
            }>
                {(1..=max_worker_count()).map(|count| view! {
                    <option value=count.to_string() selected=count == worker_count.get_untracked()>{count}</option>
                }).collect_view()}
            </select>
            </Disclosure>
            <button class="button-primary" on:click=process disabled=move || busy.get()>"Process locally"</button>
        </section>

        <section class="step" hidden=move || stage.get() != Stage::Processing>
            <h2>"Processing on this device"</h2>
            {move || {
                let done = processed.get().len();
                let total = plan.with(|plan| plan.as_ref().map(|plan| plan.groups.len()).unwrap_or(0)).max(1);
                view! {
                    <p role="status">{format!("Processing photo {} of {total}", (done + 1).min(total))}</p>
                    <progress aria-label="Processing photos" value=done.to_string() max=total.to_string()></progress>
                }
            }}
            <p>"Extracting color samples locally. Your photos stay on this device."</p>
            {move || coverage.get().map(|map| view! { <CoverageMapView map/> })}
        </section>
        <section class="step" hidden=move || stage.get() != Stage::Inspector>
            <h2>"Review what's sent"</h2>
            {move || coverage.get().map(|map| view! {
                <p class="payload-summary">{format!("{} color samples from {} {}, {:.0} KB. No pixels, no location.", points.with(Vec::len), map.images, if map.images == 1 { "photo" } else { "photos" }, processed.with(|groups| groups.iter().flat_map(|group| &group.images).map(|image| image.blob_len).sum::<usize>()) as f64 / 1024.0)}</p>
            })}
            {move || coverage.get().map(|map| view! { <CoverageMapView map/> })}
            {move || {
                let rows = failures.get();
                (!rows.is_empty()).then(|| view! {
                    <section class="excluded" aria-label="Excluded files">
                        <h3>"Files left out"</h3>
                        {excluded_files(rows, on_donate)}
                    </section>
                })
            }}
            <Disclosure summary="See exactly what's sent".to_owned()>
            <h3>"Payload inspector"</h3>
            <p>
                "Samples cannot be turned back into a photo, but someone who has the original could match them. No account or IP address is stored."
            </p>
            <p>"The views below show color samples without their positions in your photo."</p>
            <div class="inspector-canvases">
                <ColourCloud points=points/>
                <SamplesImage points=points/>
            </div>
            {move || {
                let groups = processed.get();
                let total: usize = groups.iter().flat_map(|group| group.images.iter()).map(|image| image.blob_len).sum();
                let source: u64 = groups.iter().map(|group| group.bytes_in).sum();
                view! {
                    <p>
                        {format!("{} bytes on disk → {} bytes of samples in {} image(s).", source, total, groups.iter().map(|group| group.images.len()).sum::<usize>())}
                    </p>
                }
            }}
            <h3>"Metadata sent"</h3>
            <table class="data-table">
                <thead><tr><th scope="col">Field</th><th scope="col">Value</th></tr></thead>
                <tbody>
                    {move || processed.get().iter().flat_map(|group| group.images.iter().enumerate().map(|(index, image)| {
                        let rows = metadata_rows(image);
                        let jpeg_name = group.renders.get(index).map(RenderFile::as_str);
                        let source_size = all_files.with(|files| files.iter().filter(|file| file.name() == group.raw.as_str() || jpeg_name == Some(file.name())).map(LocalFile::size).sum::<u64>());
                        view! {
                            <tr class="group-row"><th scope="row" colspan="2">{jpeg_name.unwrap_or(&group.raw).to_owned()}</th></tr>
                            <tr><th scope="row">"Size before → after"</th><td>{format!("{source_size} bytes → {} bytes", image.blob_len)}</td></tr>
                            {rows.into_iter().map(|(key, value)| view! {
                                <tr><th scope="row">{key}</th><td>{value}</td></tr>
                            }).collect_view()}
                            {image.removed.iter().map(|field| view! {
                                <tr><th scope="row">{field.clone()}</th><td class="muted">"redacted (not sent)"</td></tr>
                            }).collect_view()}
                        }
                    })).collect_view()}
                </tbody>
            </table>
            <p>"Serial: sent once, converted to a keyed hash on arrival, never stored. Choose it per batch; each batch starts with serial sending off."</p>
            <ul class="batch-serial">
                {move || batches.get().groups().iter().map(|group| {
                    let id = group.id;
                    let title = format!("Batch {}", id.0);
                    let has_serial = plan.with(|plan| plan.as_ref().is_some_and(|plan| batch_has_serial(plan, group)));
                    let checked = move || batches.with(|set| set.get(id).is_some_and(|group| group.serial_opt_in));
                    view! {
                        <li>
                            <span>{title}</span>
                            <label>
                                <input type="checkbox" prop:checked=checked disabled=!has_serial
                                    on:change=move |event| {
                                        let opt_in = event_target_checked(&event);
                                        batches.update(|set| set.set_serial(id, opt_in));
                                    }/>
                                {if has_serial { " Include my camera serial in this batch" } else { " No serial found in this batch" }}
                            </label>
                        </li>
                    }
                }).collect_view()}
            </ul>
            <p class="muted">
                "Download payload is a convenience view with a JSON rendering, not a trust anchor. To check what was actually sent, export the HAR and run "
                <code>"looklab verify-payload"</code> "."
            </p>
            <button on:click=download_payload>"Download payload (JSON, non-authoritative)"</button>
            <p>
                "Source: "
                <a href="https://github.com/skeet70/looklab">"looklab (AGPL-3.0-or-later)"</a>
                ". Prefer not to trust a served client? Use the CLI, which you can build from source."
            </p>
            <h3>"Timing"</h3>
            <table class="data-table">
                <thead><tr><th scope="col">"RAW / batch"</th><th scope="col">"Size before → after"</th><th scope="col">Time</th><th scope="col">Worker memory</th></tr></thead>
                <tbody>
                    {move || processed.get().into_iter().map(|group| view! {
                        <tr>
                            <th scope="row">{format!("{} / batch {}", group.raw, group.batch.0)}</th>
                            <td>{format!("{} bytes → {} bytes", group.bytes_in, group.images.iter().map(|image| image.blob_len).sum::<usize>())}</td>
                            <td>{if group.millis > 0.0 { format!("{:.0} ms", group.millis) } else { "Shared preparation".to_owned() }}</td>
                            <td>{format!("{:.0} MiB", group.memory as f64 / (1024.0 * 1024.0))}</td>
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
            </Disclosure>
            <button class="button-primary" on:click=move |_| current.set(StepIndex(4))>"Continue to send"</button>
        </section>
        <section class="step" hidden=move || stage.get() != Stage::Send>
            <h2>"Send your samples"</h2>
            <p>"Sign in or create a profile, then confirm once on your device. One confirmation covers every batch in this send. You can follow its progress in Your contributions."</p>
            {(!available).then(crate::ui::preview_notice)}
            <button class="button-primary" on:click=upload disabled=move || busy.get() || !available>"Send contribution"</button>
            <button on:click=download_seeds>"Export seed file (SENSITIVE)"</button>
            <p class="warn">
                "The seed file reveals sampling order. It is never uploaded; keep it local."
            </p>
            {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
            {move || send_parts.get().map(|(done, total)| view! {
                <div class="send-progress">
                    <p>{format!("Sending {} of {total} photos", done.min(total))}</p>
                    <progress aria-label="Photos sent" value=done.to_string() max=total.to_string()></progress>
                </div>
            })}
        </section>
        <section class="step contribution-done" hidden=move || stage.get() != Stage::Done>
            <h2>"Thanks, your samples are in"</h2>
            {move || sent_summary.get().map(|sent| view! {
                <p class="done-summary">{format!(
                    "{} {} from {} {}.",
                    sent.renders, if sent.renders == 1 { "photo" } else { "photos" },
                    sent.cameras, if sent.cameras == 1 { "camera" } else { "cameras" },
                )}</p>
            })}
            {move || sent_summary.get().filter(|sent| sent.deferred > 0).map(|sent| view! {
                <p class="notice" role="status">{format!(
                    "{} B&W {} received, awaiting monochrome analysis. They never count as colour samples and are kept for up to 180 days for monochrome model development.",
                    sent.deferred, if sent.deferred == 1 { "render is" } else { "renders are" },
                )}</p>
            })}
            <p>"Feedback arrives in Your contributions after processing."</p>
            <a class="button button-primary" href=crate::base::site_url("/me")>"Your contributions"</a>
            <section class="done-next" aria-labelledby="done-next-heading">
                <h3 id="done-next-heading">"Next way to help"</h3>
                <div class="help-grid">
                    {[HelpWay::WantedShots, HelpWay::Rerender].into_iter().map(|way| {
                        let guide = way.guide();
                        let href = match way {
                            HelpWay::WantedShots => crate::base::site_url("/contribute/wanted"),
                            _ => crate::base::site_url("/contribute"),
                        };
                        view! {
                            <a class="help-card help-card-link" href=href>
                                <span class="help-meta">{guide.time} " · " {guide.impact.label()}</span>
                                <span class="help-title">{guide.title}</span>
                                <span class="help-description">{guide.description}</span>
                            </a>
                        }
                    }).collect_view()}
                </div>
            </section>
            {move || {
                let rows = failures.get();
                (!rows.is_empty()).then(|| view! {
                    <section class="excluded" aria-label="Excluded files">
                        <h3>"Files left out"</h3>
                        {excluded_files(rows, on_donate)}
                    </section>
                })
            }}
            {move || {
                let links = status_links.get();
                (!links.is_empty()).then(|| view! {
                    <Disclosure summary="Keep a link to these contributions".to_owned()>
                        <ul class="status-links">
                            {links.into_iter().map(|link| view! {
                                <li><a href=link.clone()>{link.clone()}</a></li>
                            }).collect_view()}
                        </ul>
                    </Disclosure>
                })
            }}
        </section>
        </div>
    }
}

#[derive(Clone, Copy)]
enum TextField {
    Body,
    Firmware,
    Sim,
    Target,
    CardId,
    CardVersion,
    Controls,
}

impl TextField {
    fn get(self, fields: &GroupFields) -> &String {
        match self {
            Self::Body => &fields.body,
            Self::Firmware => &fields.firmware,
            Self::Sim => &fields.sim,
            Self::Target => &fields.target,
            Self::CardId => &fields.card_firmware_id,
            Self::CardVersion => &fields.card_firmware_version,
            Self::Controls => &fields.controls,
        }
    }

    fn get_mut(self, fields: &mut GroupFields) -> &mut String {
        match self {
            Self::Body => &mut fields.body,
            Self::Firmware => &mut fields.firmware,
            Self::Sim => &mut fields.sim,
            Self::Target => &mut fields.target,
            Self::CardId => &mut fields.card_firmware_id,
            Self::CardVersion => &mut fields.card_firmware_version,
            Self::Controls => &mut fields.controls,
        }
    }
}

#[derive(Clone, Copy)]
enum FlagField {
    Recipe,
    Card,
    MonochromeOptIn,
}

impl FlagField {
    fn get(self, fields: &GroupFields) -> bool {
        match self {
            Self::Recipe => fields.recipe,
            Self::Card => fields.card,
            Self::MonochromeOptIn => fields.monochrome_opt_in,
        }
    }

    fn get_mut(self, fields: &mut GroupFields) -> &mut bool {
        match self {
            Self::Recipe => &mut fields.recipe,
            Self::Card => &mut fields.card,
            Self::MonochromeOptIn => &mut fields.monochrome_opt_in,
        }
    }
}

#[component]
fn BatchEditor(
    id: BatchId,
    first: bool,
    batches: RwSignal<BatchSet>,
    firmware_ids: Vec<String>,
    on_preparation_change: Callback<()>,
) -> impl IntoView {
    let suffix = if first {
        String::new()
    } else {
        format!("-{}", id.0)
    };
    let snapshot = batches
        .with_untracked(|set| set.get(id).cloned())
        .unwrap_or_default();
    let text = move |field: TextField| {
        move || {
            batches.with(|set| {
                set.get(id)
                    .map_or_else(String::new, |group| field.get(&group.fields).clone())
            })
        }
    };
    let write = move |field: TextField| {
        move |value: String| {
            batches.update(|set| set.update(id, |group| *field.get_mut(&mut group.fields) = value));
        }
    };
    let flag = move |field: FlagField| {
        move || batches.with(|set| set.get(id).is_some_and(|group| field.get(&group.fields)))
    };
    let write_flag = move |field: FlagField| {
        move |value: bool| {
            batches.update(|set| set.update(id, |group| *field.get_mut(&mut group.fields) = value));
        }
    };

    let body = text(TextField::Body);
    let firmware = text(TextField::Firmware);
    let sim = text(TextField::Sim);
    let target = text(TextField::Target);
    let card_firmware_id = text(TextField::CardId);
    let card_firmware_version = text(TextField::CardVersion);
    let controls = text(TextField::Controls);
    let source_controls = controls;
    let write_body = write(TextField::Body);
    let write_firmware = write(TextField::Firmware);
    let write_sim = write(TextField::Sim);
    let write_target = write(TextField::Target);
    let write_card_id = write(TextField::CardId);
    let write_card_version = write(TextField::CardVersion);
    let write_controls = write(TextField::Controls);
    let recipe = flag(FlagField::Recipe);
    let card = flag(FlagField::Card);
    let monochrome_opt_in = flag(FlagField::MonochromeOptIn);
    let write_recipe = write_flag(FlagField::Recipe);
    let write_card = write_flag(FlagField::Card);
    let write_monochrome_opt_in = write_flag(FlagField::MonochromeOptIn);
    let malformed = move || batches.with(|set| set.get(id).is_some_and(|group| group.malformed));
    let menu = menu_settings(&snapshot.fields.controls);
    let raws = snapshot.raws.join(", ");
    let unavailable = snapshot.unavailable.clone();
    let unmapped = snapshot.unmapped.clone();
    let notes = snapshot.notes.clone();
    let renders = snapshot
        .renders
        .iter()
        .map(RenderFile::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let monochrome = snapshot.monochrome.clone();
    let body_id = format!("body-id{suffix}");
    let firmware_id = format!("firmware{suffix}");
    let sim_id = format!("sim{suffix}");
    let target_id = format!("recipe-target{suffix}");
    let card_id = format!("card-firmware-id{suffix}");
    let card_list_id = format!("card-firmware-ids{suffix}");
    let card_version_id = format!("card-firmware-version{suffix}");
    let controls_id = format!("controls{suffix}");

    view! {
        <div class="batch-editor">
            <p class="muted">{raws}</p>
            <p class="muted">{renders}</p>
            {(!monochrome.is_empty()).then(|| view! {
                <div class="monochrome-opt-in">
                    <p class="warn" role="status">
                        "These are B&W renders. They are left out unless you select monochrome model development below."
                    </p>
                    <label>
                        <input type="checkbox" prop:checked=monochrome_opt_in
                            on:change=move |event| {
                                write_monochrome_opt_in(event_target_checked(&event));
                                on_preparation_change.run(());
                            }/>
                        " Contribute these B&W renders for monochrome model development"
                    </label>
                    <p class="muted">
                        "looklab keeps them for up to 180 days to develop and validate monochrome response, toning and uncertainty. It deletes them after that unless they enter monochrome analysis. You can withdraw them at any time. No published Look is promised, and they never affect colour fits."
                    </p>
                </div>
            })}
            <label for=body_id.clone()>"Camera"</label>
            <input id=body_id type="text" prop:value=body
                on:input=move |event| write_body(event_target_value(&event))/>
            <label for=firmware_id.clone()>"Firmware"</label>
            <input id=firmware_id type="text" prop:value=firmware
                on:input=move |event| write_firmware(event_target_value(&event))/>
            <label for=sim_id.clone()>"Film simulation"</label>
            <input id=sim_id type="text" prop:value=sim
                on:input=move |event| write_sim(event_target_value(&event))/>
            {snapshot.family.clone().map(|family| view! {
                <p class="muted">{format!("Normalized family: {family}")}</p>
            })}
            {snapshot.mono_filter.map(|filter| view! {
                <p class="muted">{format!("Normalized monochrome filter: {filter:?}")}</p>
            })}
            <label>
                <input type="checkbox" prop:checked=recipe
                    on:change=move |event| write_recipe(event_target_checked(&event))/>
                " Settings contribution"
            </label>
            <div hidden=move || !recipe()>
                <label for=target_id.clone()>"Look these settings are trying to match"</label>
                <input id=target_id type="text" prop:value=target
                    on:input=move |event| write_target(event_target_value(&event))/>
            </div>
            <label>
                <input type="checkbox" prop:checked=card
                    on:change=move |event| write_card(event_target_checked(&event))/>
                " Rendered by custom firmware"
            </label>
            <div hidden=move || !card()>
                <p class="muted">
                    "The JPEG's custom-firmware label fills this in when available. Otherwise, enter the firmware you used."
                </p>
                <label for=card_id.clone()>"Custom firmware name"</label>
                <input id=card_id type="text" list=card_list_id.clone() prop:value=card_firmware_id
                    on:input=move |event| write_card_id(event_target_value(&event))/>
                <datalist id=card_list_id>
                    {firmware_ids.into_iter().map(|id| view! { <option value=id></option> }).collect_view()}
                </datalist>
                <label for=card_version_id.clone()>"Custom firmware version"</label>
                <input id=card_version_id type="text" prop:value=card_firmware_version
                    on:input=move |event| write_card_version(event_target_value(&event))/>
            </div>
            {move || malformed().then(|| view! {
                <p class="warn" role="alert">
                    "Some photos have an unreadable custom-firmware label. Choose custom firmware and enter its name and version, or remove those files."
                </p>
            })}
            <h3>"Camera settings"</h3>
            {menu.into_iter().map(|setting| {
                let key = setting.key;
                let read_key = key.clone();
                let label = key.as_str().replace('_', " ");
                let text = controls;
                let read = move || {
                    control_step(&text(), &read_key)
                        .map(|step| step.to_string())
                        .unwrap_or_default()
                };
                view! {
                    <label class="menu-setting">{label}
                        <input type="number" prop:value=read on:input=move |event| {
                            if let Ok(step) = event_target_value(&event).parse::<i64>() {
                                set_control_step(batches, id, &key, step);
                            }
                        }/>
                    </label>
                }
            }).collect_view()}
            {(!unmapped.is_empty()).then(|| view! {
                <p class="warn" role="alert">
                    "These settings were not readable from your files. Enter the values you used: a batch cannot be sent until each one has a value."
                </p>
                <div class="unmapped-controls">
                    {unmapped.into_iter().filter_map(|raw| looklab_schema::ControlKey::new(&raw).ok()).map(|key| {
                        let read_key = key.clone();
                        let label = key.as_str().replace('_', " ");
                        let text = controls;
                        let read = move || {
                            control_step(&text(), &read_key)
                                .map(|step| step.to_string())
                                .unwrap_or_default()
                        };
                        view! {
                            <label class="menu-setting">{label}
                                <input type="number" prop:value=read on:input=move |event| {
                                    if let Ok(step) = event_target_value(&event).parse::<i64>() {
                                        set_control_step(batches, id, &key, step);
                                    }
                                }/>
                            </label>
                        }
                    }).collect_view()}
                </div>
            })}
            <Disclosure summary="Setting sources".to_owned()>
                <ul class="setting-sources">
                    {move || {
                        let text = source_controls();
                        menu_settings(&text).into_iter().map(|setting| {
                            let key = setting.key.as_str().to_owned();
                            let step = setting.step;
                            let reproduced = batches.with(|set| {
                                set.get(id).is_some_and(|group| {
                                    group
                                        .reproduced
                                        .iter()
                                        .any(|(known, known_step)| known == &key && *known_step == step)
                                })
                            });
                            view! {
                                <li>{format!("{key} = {step}: {}", if reproduced { "reproduced from the photo" } else { "declared" })}</li>
                            }
                        }).collect_view()
                    }}
                </ul>
            </Disclosure>
            <Disclosure summary="Technical settings editor".to_owned()>
            <label for=controls_id.clone()>"Settings (key=step, one per line)"</label>
            <textarea id=controls_id rows="3" prop:value=controls
                on:input=move |event| write_controls(event_target_value(&event))></textarea>
            </Disclosure>
            {(!unavailable.is_empty()).then(|| view! {
                <p class="muted" role="status">
                    "Please check these settings, which could not be read: " {unavailable.join("; ")}
                </p>
            })}
            {(!notes.is_empty()).then(|| view! {
                <ul class="muted">
                    {notes.into_iter().map(|note| view! { <li>{note}</li> }).collect_view()}
                </ul>
            })}
        </div>
    }
}

#[derive(Clone)]
struct GroupCatalog {
    index: looklab_publish::SiteIndex,
    schemas: Vec<looklab_schema::ControlSchema>,
}

fn matching_camera<'a>(
    fact: &looklab_wasm::pairing::FileFacts,
    index: &'a looklab_publish::SiteIndex,
) -> Option<&'a looklab_publish::BodySummary> {
    let identity =
        looklab_prep::CameraIdentity::new(fact.make.as_deref()?, fact.model.as_deref()?)?;
    index
        .bodies
        .iter()
        .find(|camera| identity.matches(&camera.make, &camera.model))
}

fn title_case_words(value: &str) -> String {
    value
        .split(['-', '_'])
        .flat_map(str::split_whitespace)
        .map(|word| {
            let mut chars = word.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            first
                .to_uppercase()
                .chain(chars.flat_map(char::to_lowercase))
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn camera_display(
    fact: Option<&looklab_wasm::pairing::FileFacts>,
    catalog: Option<&GroupCatalog>,
) -> String {
    let Some(fact) = fact else {
        return "Camera not read".to_owned();
    };
    if let Some(camera) = catalog.and_then(|catalog| matching_camera(fact, &catalog.index)) {
        return format!("{} {}", camera.make, camera.model);
    }
    let make = fact
        .make
        .as_deref()
        .map(title_case_words)
        .unwrap_or_else(|| "Unknown".to_owned());
    let model = fact
        .model
        .as_deref()
        .unwrap_or("camera")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    format!("{make} {model}")
}

fn sim_menu_label(
    key: &looklab_schema::SimKey,
    fact: Option<&looklab_wasm::pairing::FileFacts>,
    catalog: Option<&GroupCatalog>,
) -> String {
    let label = (|| {
        let catalog = catalog?;
        let fact = fact?;
        let camera = matching_camera(fact, &catalog.index)?;
        let schema = catalog
            .schemas
            .iter()
            .find(|schema| schema.body == camera.id)?;
        let epoch = fact
            .firmware
            .as_deref()
            .and_then(|firmware| schema.epoch_for(firmware))
            .or_else(|| schema.epochs.first())?;
        let sim = epoch.native_sims.iter().find(|sim| sim.key == *key)?;
        catalog
            .index
            .looks
            .iter()
            .find(|look| look.slug.as_str() == sim.look.as_str())
            .map(|look| look.name.clone())
    })();
    label.unwrap_or_else(|| title_case_words(key.as_str()))
}

#[component]
fn Pairing(
    plan: PairingPlan,
    batches: RwSignal<BatchSet>,
    catalog: RwSignal<Option<GroupCatalog>>,
) -> impl IntoView {
    let PairingPlan {
        groups,
        unpaired,
        facts,
    } = plan;
    let pair_count = groups.len();
    let unpaired_count = unpaired.len();
    view! {
        <div class="pairing">
            <h3>{format!("{pair_count} {} found", if pair_count == 1 { "pair" } else { "pairs" })}</h3>
            <div class="pairing-cards">
                {groups.into_iter().map(|group| {
                    let fact = facts.iter().find(|fact| fact.name == group.raw).cloned();
                    let camera_fact = fact.clone();
                    let look_fact = fact.clone();
                    let raw = group.raw.clone();
                    view! {
                    <article class="pairing-card">
                        <h4>{move || catalog.with(|catalog| camera_display(camera_fact.as_ref(), catalog.as_ref()))}</h4>
                        <p class="help-meta">{move || {
                            let names = batches.with(|set| {
                                set.groups().iter()
                                    .filter(|batch| batch.raws.contains(&raw))
                                    .map(|batch| {
                                        looklab_schema::SimKey::new(&batch.fields.sim)
                                            .map(|key| catalog.with(|catalog| sim_menu_label(&key, look_fact.as_ref(), catalog.as_ref())))
                                            .unwrap_or_else(|_| title_case_words(&batch.fields.sim))
                                    })
                                    .collect::<Vec<_>>()
                            });
                            format!("Built-in looks: {}", names.join(", "))
                        }}</p>
                        <p><strong>{group.raw}</strong></p>
                        <p>{format!("{} paired {}", group.jpegs.len(), if group.jpegs.len() == 1 { "JPEG" } else { "JPEGs" })}</p>
                        <p class="muted">{group.jpegs.join(", ")}</p>
                    </article>
                }}).collect_view()}
            </div>
            {(!unpaired.is_empty()).then(|| view! {
                <details>
                    <summary>{format!("{unpaired_count} unpaired file(s)")}</summary>
                    <ul>
                        {unpaired.into_iter().map(|entry| view! {
                            <li>{entry.name} ": " {match entry.reason {
                                looklab_wasm::pairing::UnpairedReasonView::UnsupportedFileKind => "Not a supported RAW or JPEG file.".to_owned(),
                                looklab_wasm::pairing::UnpairedReasonView::NoCounterpart => "No matching RAW or JPEG was found.".to_owned(),
                                looklab_wasm::pairing::UnpairedReasonView::BodyMismatch => "The matching file was taken with a different camera.".to_owned(),
                                looklab_wasm::pairing::UnpairedReasonView::AmbiguousCounterpart => "More than one file could be its match. Add one pair at a time.".to_owned(),
                                looklab_wasm::pairing::UnpairedReasonView::SeparateShot => "This JPEG is from a different shot than the RAW with the same name.".to_owned(),
                                looklab_wasm::pairing::UnpairedReasonView::Unreadable(_) => "The file could not be read. Try the original camera file.".to_owned(),
                            }}</li>
                        }).collect_view()}
                    </ul>
                </details>
            })}
        </div>
    }
}

#[component]
fn CoverageMapView(map: CoverageMap) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    let bins = map.bins;
    let counts = map.counts.clone();
    let max = map.max;
    Effect::new(move |_| {
        let Some(canvas) = canvas.get() else {
            return;
        };
        let Ok(canvas) = canvas.dyn_into::<HtmlCanvasElement>() else {
            return;
        };
        canvas.set_width(bins * 6);
        canvas.set_height(bins * 6);
        let Some(context) = context(&canvas) else {
            return;
        };
        for y in 0..bins {
            for x in 0..bins {
                let count = counts[(y * bins + x) as usize];
                let level = if max == 0 {
                    0.0
                } else {
                    f64::from(count) / f64::from(max)
                };
                let value = (40.0 + level * 180.0) as u8;
                context.set_fill_style_str(&format!("rgb({value}, {value}, {value})"));
                context.fill_rect(f64::from(x * 6), f64::from(y * 6), 6.0, 6.0);
            }
        }
    });
    view! {
        <figure class="plot">
            <canvas node_ref=canvas role="img" aria-label="Coverage of processed samples"></canvas>
            <figcaption>{format!("What your photos add: {} color samples from {} {}. Brighter areas contain more samples.", map.samples, map.images, if map.images == 1 { "photo" } else { "photos" })}</figcaption>
        </figure>
    }
}

#[component]
fn ColourCloud(points: RwSignal<Vec<[f32; 3]>>) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    Effect::new(move |_| {
        points.with(|points| draw_points(&canvas, points, Projection::Cloud));
    });
    view! {
        <figure class="plot">
            <canvas node_ref=canvas role="img" aria-label="3D colour cloud of the samples that leave"></canvas>
            <figcaption>"Exactly the samples that leave (canvas projection)"</figcaption>
        </figure>
    }
}

#[component]
fn SamplesImage(points: RwSignal<Vec<[f32; 3]>>) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    Effect::new(move |_| {
        points.with(|points| draw_points(&canvas, points, Projection::Image));
    });
    view! {
        <figure class="plot">
            <canvas node_ref=canvas role="img" aria-label="Samples laid out as an image in upload order"></canvas>
            <figcaption>"Can you see the photo? Samples in upload order look like noise."</figcaption>
        </figure>
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Projection {
    Cloud,
    Image,
}

fn draw_points(node: &NodeRef<Canvas>, points: &[[f32; 3]], projection: Projection) {
    let Some(canvas) = node.get() else {
        return;
    };
    let Ok(canvas) = canvas.dyn_into::<HtmlCanvasElement>() else {
        return;
    };
    let side = ((points.len() as f64).sqrt().ceil() as u32).max(1);
    let (width, height) = match projection {
        Projection::Cloud => (320, 240),
        Projection::Image => (side, side),
    };
    canvas.set_width(width);
    canvas.set_height(height);
    let Some(context) = context(&canvas) else {
        return;
    };
    context.set_fill_style_str("#101010");
    context.fill_rect(0.0, 0.0, f64::from(width), f64::from(height));
    if projection == Projection::Image {
        let pixel = 1;
        for (index, rgb) in points.iter().enumerate() {
            let x = (index as u32 % side) * pixel;
            let y = (index as u32 / side) * pixel;
            context.set_fill_style_str(&colour(*rgb));
            context.fill_rect(
                f64::from(x),
                f64::from(y),
                f64::from(pixel),
                f64::from(pixel),
            );
        }
        return;
    }
    for rgb in points {
        let x = 0.5 + f64::from(rgb[0] - rgb[2]) * 0.45;
        let y = 0.7 + f64::from(rgb[0] + rgb[2]) * 0.15 - f64::from(rgb[1]) * 0.6;
        let px = x * f64::from(width);
        let py = y * f64::from(height);
        context.set_fill_style_str(&colour(*rgb));
        context.fill_rect(px, py, 1.5, 1.5);
    }
}

fn colour(rgb: [f32; 3]) -> String {
    let channel = |value: f32| -> u8 { (value.clamp(0.0, 1.0) * 255.0).round() as u8 };
    format!(
        "rgb({}, {}, {})",
        channel(rgb[0]),
        channel(rgb[1]),
        channel(rgb[2])
    )
}

fn context(canvas: &HtmlCanvasElement) -> Option<CanvasRenderingContext2d> {
    canvas
        .get_context("2d")
        .ok()
        .flatten()?
        .dyn_into::<CanvasRenderingContext2d>()
        .ok()
}

fn metadata_rows(image: &PrepSummary) -> Vec<(String, String)> {
    let value = serde_json::to_value(&image.metadata).unwrap_or(serde_json::Value::Null);
    let Some(object) = value.as_object() else {
        return Vec::new();
    };
    object
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(key, value)| (key.clone(), value.to_string()))
        .collect()
}

struct SiteBody {
    body: String,
    yaml: String,
    schema: looklab_schema::ControlSchema,
}

/// Reads each pair's settings from its makernotes, keeping only the latest answer per file and drop.
async fn read_pairs(
    pairing: &PairingPlan,
    files: &[LocalFile],
    parsed: &[(String, ParsedMetadata)],
    ledger: StoredValue<PrefillLedger, leptos::prelude::LocalStorage>,
    epoch: u64,
) -> Vec<PairReading> {
    let mut bodies: HashMap<(String, String), Result<SiteBody, String>> = HashMap::new();
    let mut readings = Vec::new();
    for pair in &pairing.groups {
        for name in &pair.jpegs {
            let Some(render) = RenderFile::new(name) else {
                continue;
            };
            let Some(ticket) =
                ledger.try_update_value(|ledger| ledger.begin(epoch, render.as_str()))
            else {
                return readings;
            };
            let reading =
                reading_for_render(pairing, pair, &render, files, parsed, &mut bodies).await;
            if ledger.with_value(|ledger| ledger.accept(&ticket)) {
                readings.push(reading);
            }
        }
    }
    readings
}

async fn reading_for_render(
    pairing: &PairingPlan,
    pair: &looklab_wasm::PairGroup,
    render: &RenderFile,
    files: &[LocalFile],
    parsed: &[(String, ParsedMetadata)],
    bodies: &mut HashMap<(String, String), Result<SiteBody, String>>,
) -> PairReading {
    let facts = [pair.raw.as_str(), render.as_str()]
        .into_iter()
        .filter_map(|name| pairing.facts.iter().find(|fact| fact.name == name))
        .collect::<Vec<_>>();
    let mut reading = PairReading {
        raw: pair.raw.clone(),
        render: render.clone(),
        camera: facts.first().map(|fact| CameraKey {
            make: fact.make.clone(),
            model: fact.model.clone(),
        }),
        body: String::new(),
        epoch: None,
        firmware: facts
            .iter()
            .find(|fact| fact.name == render.as_str())
            .and_then(|fact| fact.firmware.clone())
            .or_else(|| facts.first().and_then(|fact| fact.firmware.clone()))
            .unwrap_or_default(),
        sim: None,
        controls: Vec::new(),
        unavailable: Vec::new(),
        unmapped: Vec::new(),
        card: facts
            .iter()
            .find(|fact| fact.name == render.as_str())
            .and_then(|fact| fact.render_tag.clone()),
        malformed: facts
            .iter()
            .any(|fact| fact.name == render.as_str() && fact.render_tag_malformed),
        render_class: RenderClass::Colour,
        family: None,
        mono_filter: None,
        monochrome: Vec::new(),
        note: None,
    };
    let Some((make, model)) = reading
        .camera
        .as_ref()
        .and_then(|camera| Some((camera.make.clone()?, camera.model.clone()?)))
    else {
        reading.note = Some("the files do not name a camera".to_owned());
        return reading;
    };
    if find_file(files, &pair.raw).is_none() {
        reading.note = Some("the RAW is no longer available".to_owned());
        return reading;
    }
    let key = (make.trim().to_lowercase(), model.trim().to_lowercase());
    if !bodies.contains_key(&key) {
        bodies.insert(key.clone(), site_body(&make, &model).await);
    }
    let site = match bodies.get(&key) {
        Some(Ok(site)) => site,
        Some(Err(error)) => {
            reading.note = Some(error.clone());
            return reading;
        }
        None => return reading,
    };
    reading.body = site.body.clone();
    if find_file(files, render.as_str()).is_none() {
        reading.note = Some("the JPEG is no longer available".to_owned());
        return reading;
    }
    let pair_files = [pair.raw.as_str(), render.as_str()]
        .into_iter()
        .filter_map(|name| {
            parsed
                .iter()
                .find(|(known, _)| known.as_str() == name)
                .map(|(known, metadata)| (known.as_str(), metadata))
        })
        .collect::<Vec<_>>();
    match prefill_settings_parsed(&site.yaml, &pair_files) {
        Ok(prefill) => {
            let epoch = site.schema.epoch_for(&prefill.firmware);
            if let Some(epoch) = epoch
                && let Ok(version) = prefill.firmware.parse::<looklab_schema::FirmwareVersion>()
                && (version < epoch.firmware.from || version > epoch.firmware.to)
            {
                reading.note = Some(format!(
                    "Firmware {} uses epoch {} provisionally; its published range is {} to {}.",
                    prefill.firmware, epoch.id, epoch.firmware.from, epoch.firmware.to
                ));
            }
            reading.epoch = epoch.map(|epoch| epoch.id);
            reading.firmware = prefill.firmware;
            reading.sim = prefill.sim;
            reading.controls = prefill.controls;
            reading.unavailable = prefill.unavailable;
            reading.unmapped = prefill.unmapped;
            reading.family = prefill.family;
            reading.mono_filter = prefill.mono_filter;
            reading.monochrome = prefill.monochrome;
            reading.render_class = match prefill.render_class {
                Some(looklab_schema::RenderClass::Monochrome) => RenderClass::Monochrome,
                _ => RenderClass::Colour,
            };
            if let Some(tag) = &reading.card
                && let Some(epoch) = epoch
                && let Ok(family) = looklab_schema::LookId::new(tag.look_slug.as_str())
                && let Some(declared) = epoch.family_render_class(&family)
                && let Some(fuji) = parsed
                    .iter()
                    .find(|(name, _)| name.as_str() == render.as_str())
                    .and_then(|(_, metadata)| metadata.fuji.as_ref())
            {
                let values = looklab_schema::MakernoteValues::from(&fuji.look);
                if let Ok(classified) =
                    looklab_schema::classify_card_render(epoch, &family, declared, &values)
                {
                    reading.mono_filter = classified.mono_filter;
                    reading.render_class = match classified.render_class {
                        looklab_schema::RenderClass::Monochrome => RenderClass::Monochrome,
                        looklab_schema::RenderClass::Colour => RenderClass::Colour,
                    };
                    let (settings, problems) = looklab_schema::settings_from_makernote_for(
                        &site.schema,
                        epoch.id,
                        &classified.family,
                        &values,
                    );
                    reading.controls = settings
                        .entries()
                        .iter()
                        .map(|setting| (setting.key.to_string(), setting.step))
                        .collect();
                    let reach = looklab_schema::applicable_controls(epoch, &classified.family);
                    reading.family = Some(classified.family);
                    let reproduced = reading
                        .controls
                        .iter()
                        .map(|(key, _)| key.as_str())
                        .collect::<Vec<_>>();
                    let mut unmapped = reach
                        .mapped
                        .iter()
                        .chain(&reach.unmapped)
                        .filter(|key| !reproduced.contains(&key.as_str()))
                        .map(ToString::to_string)
                        .collect::<Vec<_>>();
                    if reading.render_class == RenderClass::Monochrome {
                        for key in &reach.spatial {
                            if !reproduced.contains(&key.as_str())
                                && !unmapped.iter().any(|known| known.as_str() == key.as_str())
                            {
                                unmapped.push(key.to_string());
                            }
                        }
                    }
                    reading.unmapped = unmapped;
                    reading.unavailable = problems.iter().map(ToString::to_string).collect();
                    let sim = reading.sim.clone().unwrap_or_default();
                    if reading.render_class == RenderClass::Monochrome {
                        if !reading
                            .monochrome
                            .iter()
                            .any(|known| known.file == render.as_str())
                        {
                            reading.monochrome.push(looklab_wasm::MonochromeRender {
                                file: render.as_str().to_owned(),
                                sim,
                            });
                        }
                    } else {
                        reading
                            .monochrome
                            .retain(|known| known.file != render.as_str());
                    }
                }
            }
        }
        Err(error) => reading.note = Some(error.to_string()),
    }
    reading
}

async fn site_body(make: &str, model: &str) -> Result<SiteBody, String> {
    let index = data::index().await?;
    let identity = looklab_prep::CameraIdentity::new(make, model);
    let summary = index
        .bodies
        .iter()
        .find(|body| {
            identity
                .as_ref()
                .is_some_and(|identity| identity.matches(&body.make, &body.model))
        })
        .ok_or_else(|| format!("the site data has no camera for {make} {model}"))?;
    let page = data::body(summary.id.make(), summary.id.model()).await?;
    let schema = looklab_schema::validate_control_schema(&page.schema).map_err(|errors| {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("; ")
    })?;
    Ok(SiteBody {
        body: page.id.to_string(),
        yaml: page.schema,
        schema,
    })
}

struct DraftGroup<'a> {
    body: &'a str,
    firmware: &'a str,
    sim: &'a str,
    controls: &'a str,
    reproduced: &'a [(String, i64)],
    unmapped: &'a [String],
    snapshot_version: u64,
    recipe: bool,
    target: &'a str,
    card: bool,
    tag: Option<&'a RenderTag>,
    malformed: bool,
    card_firmware_id: &'a str,
    card_firmware_version: &'a str,
    monochrome: bool,
    monochrome_opt_in: bool,
}

fn group_settings(draft: DraftGroup<'_>) -> Result<GroupSettings, String> {
    let render_class = if draft.monochrome {
        RenderClass::Monochrome
    } else {
        RenderClass::Colour
    };
    if draft.monochrome && !draft.monochrome_opt_in {
        return Err(
            "Select monochrome model development to send this batch's B&W renders.".to_owned(),
        );
    }
    let monochrome_authorization = (render_class == RenderClass::Monochrome)
        .then_some(MonochromeAuthorization::ModelDevelopment180Days);
    let mut entries = Vec::new();
    for line in draft
        .controls
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let (key, step) = line
            .split_once('=')
            .ok_or_else(|| format!("{line}: expected key=step"))?;
        let key = key.trim();
        let step = step
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("{line}: step is not an integer"))?;
        let source = if draft
            .reproduced
            .iter()
            .any(|(reproduced, reproduced_step)| reproduced == key && *reproduced_step == step)
        {
            SettingSource::Reproduced
        } else {
            SettingSource::Declared
        };
        entries.push(ControlSetting {
            key: key.to_owned(),
            step,
            source,
        });
    }
    let missing = draft
        .unmapped
        .iter()
        .filter(|key| !entries.iter().any(|entry| &entry.key == *key))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "This batch still needs a value for {}. Enter the value you used under Camera settings, or remove those photos from the batch.",
            missing.join(", ")
        ));
    }
    let intent = if let Some(tag) = draft.tag {
        IntentChoice::Card(CardDeclaration::tagged(tag.clone()))
    } else if draft.card {
        let id = FirmwareId::new(draft.card_firmware_id.trim())
            .map_err(|error| format!("card firmware id: {error}"))?;
        let version = FirmwareVersion::new(draft.card_firmware_version.trim())
            .map_err(|error| format!("card firmware version: {error}"))?;
        IntentChoice::Card(CardDeclaration::user_declared(id, version))
    } else if draft.malformed {
        return Err(
            "One or more images carry a malformed card render tag. Select Card contribution and declare the firmware, or remove those files."
                .to_owned(),
        );
    } else if draft.recipe {
        IntentChoice::Recipe(draft.target.trim().to_owned())
    } else {
        IntentChoice::Native
    };
    Ok(GroupSettings {
        kind: ContributionKind::Library,
        intent,
        body: draft.body.trim().to_owned(),
        firmware: draft.firmware.trim().to_owned(),
        sim: draft.sim.trim().to_owned(),
        community_snapshot_version: draft.snapshot_version,
        controls: entries,
        render_class,
        monochrome_authorization,
    })
}

fn prepared_images<'a>(
    groups: impl IntoIterator<Item = &'a ProcessedGroup>,
) -> Vec<PreparedImageView> {
    let mut images = Vec::new();
    for group in groups {
        for (index, summary) in group.images.iter().enumerate() {
            if let Some(blob) = group.blobs.get(index) {
                images.push(PreparedImageView {
                    blob: blob.clone(),
                    metadata: summary.metadata.clone(),
                    removed: summary.removed.clone(),
                    rerender_group: summary.rerender_group,
                    alignment: summary.alignment,
                });
            }
        }
    }
    images
}

fn prepared_images_of(groups: &[ProcessedGroup], batch: BatchId) -> Vec<PreparedImageView> {
    prepared_images(groups.iter().filter(|group| group.batch == batch))
}

fn group_settings_for(
    group: &ContributionGroup,
    images: &[PreparedImageView],
    snapshot_version: u64,
) -> Result<GroupSettings, String> {
    let card_tag = images
        .iter()
        .find_map(|image| image.metadata.render_tag.clone());
    let malformed = group.malformed
        || images
            .iter()
            .any(|image| image.metadata.render_tag_malformed);
    group_settings(DraftGroup {
        body: &group.fields.body,
        firmware: &group.fields.firmware,
        sim: &group.fields.sim,
        controls: &group.fields.controls,
        reproduced: &group.reproduced,
        unmapped: &group.unmapped,
        snapshot_version,
        recipe: group.fields.recipe,
        target: &group.fields.target,
        card: group.fields.card || card_tag.is_some(),
        tag: card_tag.as_ref(),
        malformed,
        card_firmware_id: &group.fields.card_firmware_id,
        card_firmware_version: &group.fields.card_firmware_version,
        monochrome: group.is_monochrome(),
        monochrome_opt_in: group.fields.monochrome_opt_in,
    })
}

fn batch_has_serial(pairing: &PairingPlan, group: &ContributionGroup) -> bool {
    pairing
        .groups
        .iter()
        .filter(|pair| group.raws.iter().any(|raw| raw == &pair.raw))
        .any(|pair| {
            std::iter::once(&pair.raw)
                .chain(pair.jpegs.iter())
                .any(|name| {
                    pairing
                        .facts
                        .iter()
                        .any(|fact| &fact.name == name && fact.has_serial)
                })
        })
}

pub(crate) async fn run_files(
    pairing: &PairingPlan,
    files: &[LocalFile],
    count: u32,
) -> Result<(Vec<ProcessedGroup>, Vec<SeedRow>, Vec<FailureRow>), String> {
    let batches = BatchSet::covering(pairing);
    run_files_progress(pairing, &batches, files, count, None).await
}

enum PairOutcome {
    Prepared {
        groups: Vec<ProcessedGroup>,
        seed: SeedRow,
    },
    Failed(FailureRow),
}

/// The pair's usable JPEGs, and a listing row for every monochrome render.
fn split_renders(
    pair: &looklab_wasm::PairGroup,
    excluded: &HashSet<String>,
) -> (Vec<RenderFile>, Vec<FailureRow>) {
    let mut usable = Vec::with_capacity(pair.jpegs.len());
    let mut rows = Vec::new();
    for name in &pair.jpegs {
        if excluded.contains(name) {
            rows.push(FailureRow {
                name: name.clone(),
                message: MONOCHROME_UNSELECTED.to_owned(),
                donated: false,
            });
        } else if let Some(render) = RenderFile::new(name) {
            usable.push(render);
        } else {
            rows.push(FailureRow {
                name: name.clone(),
                message: "the JPEG file name is invalid".to_owned(),
                donated: false,
            });
        }
    }
    (usable, rows)
}

async fn prepare_pair(
    worker: &mut PrepWorker,
    pair: &looklab_wasm::PairGroup,
    jpegs: &[RenderFile],
    files: &[LocalFile],
    batches: &BatchSet,
    progress: &Option<Callback<ProcessedGroup>>,
) -> PairOutcome {
    let failed = |name: String, message: &str| {
        PairOutcome::Failed(FailureRow {
            name,
            message: message.to_owned(),
            donated: false,
        })
    };
    let Some(raw) = find_file(files, &pair.raw) else {
        return failed(pair.raw.clone(), "the file is no longer available");
    };
    let raw_size = raw.size();
    let raw_bytes = match raw.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => return failed(pair.raw.clone(), &error),
    };
    let mut jpeg_bytes = Vec::with_capacity(jpegs.len());
    let mut jpeg_sizes = Vec::with_capacity(jpegs.len());
    for name in jpegs {
        let Some(file) = find_file(files, name.as_str()) else {
            return failed(name.as_str().to_owned(), "the file is no longer available");
        };
        jpeg_sizes.push(file.size());
        match file.bytes().await {
            Ok(bytes) => jpeg_bytes.push(bytes),
            Err(error) => return failed(name.as_str().to_owned(), &error),
        }
    }
    if jpeg_bytes.is_empty() {
        return failed(pair.raw.clone(), "no JPEG is paired with this RAW");
    }
    let mut seed = [0u8; 32];
    let Some(crypto) = web_sys::window().and_then(|window| window.crypto().ok()) else {
        return failed(pair.raw.clone(), "crypto.getRandomValues is unavailable");
    };
    if crypto.get_random_values_with_u8_array(&mut seed).is_err() {
        return failed(pair.raw.clone(), "crypto.getRandomValues failed");
    }
    let request = prepare_request(raw_bytes, jpeg_bytes, PRIOR_VERSION, seed);
    let start = performance_now();
    let reply = worker
        .call(module_fn_id(ModuleFunction::Prepare), request)
        .await;
    let millis = performance_now() - start;
    match reply {
        Ok(reply) => match decode_prepare_reply(&reply.bytes) {
            Ok(response) => {
                if response.images.len() != jpegs.len() {
                    return failed(
                        pair.raw.clone(),
                        "the prepared renders do not match the JPEGs",
                    );
                }
                let mut groups: Vec<ProcessedGroup> = Vec::new();
                let mut slots: HashMap<BatchId, usize> = HashMap::new();
                for (index, (render, image)) in jpegs.iter().zip(response.images).enumerate() {
                    let Some(batch) = batches.batch_of(render) else {
                        return failed(
                            render.as_str().to_owned(),
                            "the JPEG has no confirmed batch",
                        );
                    };
                    let slot = *slots.entry(batch).or_insert_with(|| {
                        let first = groups.is_empty();
                        let slot = groups.len();
                        groups.push(ProcessedGroup {
                            raw: pair.raw.clone(),
                            renders: Vec::new(),
                            batch,
                            images: Vec::new(),
                            blobs: Vec::new(),
                            bytes_in: if first { raw_size } else { 0 },
                            millis: if first { millis } else { 0.0 },
                            memory: reply.memory,
                        });
                        slot
                    });
                    let group = &mut groups[slot];
                    group.renders.push(render.clone());
                    group.bytes_in += jpeg_sizes[index];
                    group.images.push(PrepSummary {
                        blob_len: image.blob.len(),
                        metadata: image.metadata,
                        removed: image.removed,
                        rerender_group: image.rerender_group,
                        alignment: image.alignment,
                    });
                    group.blobs.push(image.blob);
                }
                if let Some(progress) = progress {
                    for group in &groups {
                        progress.run(group.clone());
                    }
                }
                PairOutcome::Prepared {
                    groups,
                    seed: SeedRow {
                        name: pair.raw.clone(),
                        seed,
                    },
                }
            }
            Err(error) => failed(pair.raw.clone(), &error.to_string()),
        },
        Err(error) => failed(pair.raw.clone(), &error.message()),
    }
}

fn lane_indices(count: usize, lanes: usize) -> Vec<Vec<usize>> {
    let mut lanes: Vec<Vec<usize>> = (0..lanes.max(1).min(count.max(1)))
        .map(|_| Vec::new())
        .collect();
    let lane_count = lanes.len();
    for index in 0..count {
        lanes[index % lane_count].push(index);
    }
    lanes
}

async fn validate_batch_sims(batches: &BatchSet) -> Result<(), String> {
    let mut schemas = HashMap::new();
    for group in batches.groups() {
        if group.renders.is_empty() {
            continue;
        }
        let monochrome = group.is_monochrome();
        if monochrome && !group.fields.monochrome_opt_in {
            continue;
        }
        let body =
            looklab_upload::BodyId::new(&group.fields.body).map_err(|error| error.to_string())?;
        let sim =
            looklab_schema::SimKey::new(&group.fields.sim).map_err(|error| error.to_string())?;
        let schema = match schemas.entry(body) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let page = data::body(entry.key().make(), entry.key().model()).await?;
                let schema =
                    looklab_schema::validate_control_schema(&page.schema).map_err(|errors| {
                        errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join("; ")
                    })?;
                entry.insert(schema)
            }
        };
        let epoch = schema.epoch_for(&group.fields.firmware).ok_or_else(|| {
            "This firmware isn't listed in the camera's published schema.".to_owned()
        })?;
        let native = epoch
            .native_sims
            .iter()
            .find(|native| native.key == sim)
            .ok_or_else(|| {
                "This built-in look isn't listed in the camera's published schema.".to_owned()
            })?;
        if monochrome && !group.fields.card && !native.monochrome {
            return Err(
                "This batch holds B&W renders but declares a colour film simulation. Enter the B&W film simulation the camera recorded."
                    .to_owned(),
            );
        }
        if monochrome {
            let family = group.family.as_ref().ok_or_else(|| {
                "This B&W batch could not be classified against the camera's published schema."
                    .to_owned()
            })?;
            if !epoch.supports_monochrome_intake(family) {
                return Err(
                    "This camera's published schema does not accept B&W measurements yet."
                        .to_owned(),
                );
            }
        }
        if !monochrome && !group.fields.card && native.monochrome {
            return Err(
                "This batch declares a B&W film simulation but none of its renders are B&W. Enter the colour film simulation the camera recorded."
                    .to_owned(),
            );
        }
    }
    Ok(())
}

async fn run_files_progress(
    pairing: &PairingPlan,
    batches: &BatchSet,
    files: &[LocalFile],
    count: u32,
    progress: Option<Callback<ProcessedGroup>>,
) -> Result<(Vec<ProcessedGroup>, Vec<SeedRow>, Vec<FailureRow>), String> {
    if pairing.groups.is_empty() {
        return Ok((Vec::new(), Vec::new(), Vec::new()));
    }
    let lanes = lane_indices(
        pairing.groups.len(),
        usize::try_from(count.max(1)).unwrap_or(1),
    );
    let monochrome: HashSet<String> = batches
        .groups()
        .iter()
        .flat_map(|group| &group.monochrome)
        .map(|render| render.file.clone())
        .collect();
    let selected: HashSet<String> = batches
        .groups()
        .iter()
        .filter(|group| group.fields.monochrome_opt_in)
        .flat_map(|group| &group.monochrome)
        .map(|render| render.file.clone())
        .collect();
    let excluded: HashSet<String> = monochrome.difference(&selected).cloned().collect();
    let mut futures: Vec<Lane<'_, PairOutcomes>> = Vec::with_capacity(lanes.len());
    for indices in lanes {
        let excluded = &excluded;
        futures.push(Box::pin(async move {
            let mut worker = PrepWorker::spawn()?;
            let mut outcomes = Vec::with_capacity(indices.len());
            for index in indices {
                if worker.should_recycle() {
                    worker.terminate();
                    worker = PrepWorker::spawn()?;
                }
                let pair = &pairing.groups[index];
                let (usable, refusals) = split_renders(pair, excluded);
                let outcome = if usable.is_empty() && !refusals.is_empty() {
                    PairOutcome::Failed(FailureRow {
                        name: pair.raw.clone(),
                        message: MONOCHROME_UNSELECTED.to_owned(),
                        donated: false,
                    })
                } else {
                    prepare_pair(&mut worker, pair, &usable, files, batches, &progress).await
                };
                outcomes.push((index, outcome, refusals));
            }
            worker.terminate();
            Ok(outcomes)
        }));
    }
    let mut outcomes = Vec::new();
    for lane in join_all(futures).await {
        outcomes.extend(lane?);
    }
    outcomes.sort_by_key(|(index, ..)| *index);
    let mut groups = Vec::new();
    let mut seed_rows = Vec::new();
    let mut failures = Vec::new();
    for (_, outcome, refusals) in outcomes {
        failures.extend(refusals);
        match outcome {
            PairOutcome::Prepared {
                groups: prepared,
                seed,
            } => {
                groups.extend(prepared);
                seed_rows.push(seed);
            }
            PairOutcome::Failed(row) => failures.push(row),
        }
    }
    let mut card_failures = check_card_tags(&mut groups).await;
    failures.append(&mut card_failures);
    Ok((groups, seed_rows, failures))
}

type Lane<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
type PairOutcomes = Result<Vec<(usize, PairOutcome, Vec<FailureRow>)>, String>;

struct JoinAll<'a, T> {
    futures: Vec<Lane<'a, T>>,
    results: Vec<Option<T>>,
}

impl<T: Unpin> Future for JoinAll<'_, T> {
    type Output = Vec<T>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let mut ready = 0;
        for (index, future) in this.futures.iter_mut().enumerate() {
            if this.results[index].is_some() {
                ready += 1;
                continue;
            }
            if let Poll::Ready(value) = future.as_mut().poll(context) {
                this.results[index] = Some(value);
                ready += 1;
            }
        }
        if ready == this.futures.len() {
            Poll::Ready(this.results.iter_mut().filter_map(Option::take).collect())
        } else {
            Poll::Pending
        }
    }
}

async fn join_all<'a, T: Unpin>(futures: Vec<Lane<'a, T>>) -> Vec<T> {
    JoinAll {
        results: futures.iter().map(|_| None).collect(),
        futures,
    }
    .await
}

async fn check_card_tags(groups: &mut [ProcessedGroup]) -> Vec<FailureRow> {
    let mut expected: HashMap<BatchId, RenderTag> = HashMap::new();
    for group in groups.iter() {
        let tag = group
            .images
            .iter()
            .find_map(|image| image.metadata.render_tag.clone());
        if let Some(tag) = tag {
            expected.entry(group.batch).or_insert(tag);
        }
    }
    let mut failures = Vec::new();
    for group in groups {
        let batch_tag = expected.get(&group.batch);
        let mut drop = Vec::new();
        for (index, image) in group.images.iter().enumerate() {
            if image.metadata.render_tag_malformed {
                if batch_tag.is_some() {
                    drop.push((
                        index,
                        "the render tag is malformed; declare the firmware or exclude this file"
                            .to_owned(),
                    ));
                }
                continue;
            }
            let Some(tag) = image.metadata.render_tag.as_ref() else {
                continue;
            };
            if batch_tag.is_some_and(|first| first != tag) {
                drop.push((
                    index,
                    "the render tag differs from the first tag in this batch".to_owned(),
                ));
                continue;
            }
            match data::card(tag.look_slug.as_str(), tag.look_version.as_str()).await {
                Ok(bytes) => match check_card(tag, &bytes) {
                    Ok(()) => {}
                    Err(CardCheckError::CidMismatch) => drop.push((
                        index,
                        format!(
                            "cid {} does not match the published card for {}@{}",
                            tag.cid_prefix_hex(),
                            tag.look_slug,
                            tag.look_version
                        ),
                    )),
                    Err(error) => drop.push((index, error.to_string())),
                },
                Err(error) => drop.push((index, error)),
            }
        }
        for (index, reason) in drop.into_iter().rev() {
            group.blobs.remove(index);
            group.images.remove(index);
            let render = group.renders.remove(index);
            failures.push(FailureRow {
                name: render.as_str().to_owned(),
                message: format!("excluded: {reason}"),
                donated: false,
            });
        }
    }
    failures
}

fn group_failures(rows: Vec<FailureRow>) -> Vec<(String, Vec<FailureRow>)> {
    let mut groups: Vec<(String, Vec<FailureRow>)> = Vec::new();
    for row in rows {
        match groups
            .iter_mut()
            .find(|(message, _)| *message == row.message)
        {
            Some((_, rows)) => rows.push(row),
            None => groups.push((row.message.clone(), vec![row])),
        }
    }
    groups
}

fn excluded_files(rows: Vec<FailureRow>, on_donate: Callback<String>) -> impl IntoView {
    group_failures(rows)
        .into_iter()
        .map(|(message, rows)| {
            let count = rows.len();
            view! {
                <details class="disclosure excluded-files">
                    <summary>{format!("{count} {} left out: {message}", if count == 1 { "file" } else { "files" })}</summary>
                    <ul>
                        {rows.into_iter().map(|row| {
                            let name = row.name.clone();
                            view! {
                                <li>
                                    <span>{row.name.clone()}</span>
                                    {(!row.donated).then(|| view! {
                                        <button type="button" class="excluded-donate"
                                            on:click=move |_| on_donate.run(name.clone())>
                                            "Donate this file as a test fixture"
                                        </button>
                                    })}
                                </li>
                            }
                        }).collect_view()}
                    </ul>
                </details>
            }
        })
        .collect_view()
}

fn sent_summary_of(pending: &[PendingBatch]) -> SentSummary {
    let mut cameras = pending
        .iter()
        .map(|batch| batch.settings.body.as_str())
        .collect::<Vec<_>>();
    cameras.sort_unstable();
    cameras.dedup();
    SentSummary {
        renders: pending.iter().map(|batch| batch.images.len()).sum(),
        deferred: pending
            .iter()
            .filter(|batch| batch.settings.render_class == RenderClass::Monochrome)
            .map(|batch| batch.images.len())
            .sum(),
        cameras: cameras.len(),
    }
}

struct PendingBatch {
    id: BatchId,
    settings: GroupSettings,
    images: Vec<PreparedImageView>,
    serial: Option<Vec<u8>>,
}

struct SendOutcome {
    accepted: Vec<(BatchId, webauthn::UploadReply)>,
    error: Option<(usize, String)>,
}

async fn upload_send(
    session: crate::session::SessionContext,
    pending: Vec<PendingBatch>,
    send_parts: RwSignal<Option<(usize, usize)>>,
) -> SendOutcome {
    let total = pending.len();
    let total_photos = pending
        .iter()
        .map(|batch| batch.images.len())
        .sum::<usize>();
    let mut photos_sent = 0;
    let mut accepted = Vec::new();
    let error = |position: usize, message: String| SendOutcome {
        accepted: Vec::new(),
        error: Some((position, message)),
    };
    // Once a batch is uploaded, a later failure must keep it; a retry skips accepted batches.
    macro_rules! fail {
        ($position:expr, $message:expr) => {
            return SendOutcome {
                accepted,
                error: Some(($position, $message)),
            }
        };
    }
    if pending.iter().any(|batch| batch.images.is_empty()) {
        return error(0, "nothing to upload".to_owned());
    }
    if session.require_profile().await.is_none() {
        return error(
            0,
            "Sign in was cancelled. Your prepared samples are still here.".to_owned(),
        );
    }
    let credential_id = match idb::get(idb::StorageKey::CREDENTIAL).await {
        Ok(id) => id.and_then(|text| webauthn::decode_base64url(&text)),
        Err(message) => return error(0, message),
    };
    let nonce = match webauthn::fetch_nonce().await {
        Ok(nonce) => nonce,
        Err(message) => return error(0, message),
    };
    let mut worker = match PrepWorker::spawn() {
        Ok(worker) => worker,
        Err(message) => return error(0, message),
    };

    let mut manifests = Vec::with_capacity(total);
    for batch in &pending {
        let request = match build_manifest_request(&batch.settings, &batch.images, nonce.nonce) {
            Ok(request) => request,
            Err(failure) => return error(0, failure.to_string()),
        };
        let reply = match worker
            .call(module_fn_id(ModuleFunction::BuildManifest), request)
            .await
        {
            Ok(reply) => reply,
            Err(failure) => return error(0, failure.message()),
        };
        match decode_build_manifest_reply(&reply.bytes) {
            Ok(manifest) => manifests.push(manifest),
            Err(failure) => return error(0, failure.to_string()),
        }
    }
    let hashes = manifests
        .iter()
        .map(|manifest| manifest.manifest_hash)
        .collect::<Vec<_>>();
    let send_request = match build_send_manifest_request(nonce.nonce, hashes) {
        Ok(request) => request,
        Err(failure) => return error(0, failure.to_string()),
    };
    let send_reply = match worker
        .call(
            module_fn_id(ModuleFunction::BuildSendManifest),
            send_request,
        )
        .await
    {
        Ok(reply) => reply,
        Err(failure) => return error(0, failure.message()),
    };
    let send = match decode_build_send_manifest_reply(&send_reply.bytes) {
        Ok(send) => send,
        Err(failure) => return error(0, failure.to_string()),
    };
    let assertion = match webauthn::sign_batch(
        send.challenge.as_bytes(),
        credential_id.as_deref(),
        &nonce.rp_id,
    )
    .await
    {
        Ok(assertion) => assertion,
        Err(message) => return error(0, message),
    };
    let defaults = default_pow();
    let pow = nonce.pow.unwrap_or(webauthn::PowParameters {
        memory_kib: defaults.memory_kib,
        iterations: defaults.iterations,
        difficulty_bits_per_image: defaults.difficulty_bits_per_image,
    });

    for (position, (batch, manifest)) in pending.iter().zip(&manifests).enumerate() {
        let n_images = match u32::try_from(batch.images.len()) {
            Ok(count) => count,
            Err(_) => fail!(position, "too many images".to_owned()),
        };
        let pow_request = match solve_pow_request(
            *manifest.challenge.as_bytes(),
            n_images,
            PowParamsSpec {
                memory_kib: pow.memory_kib,
                iterations: pow.iterations,
                difficulty_bits_per_image: pow.difficulty_bits_per_image,
            },
            0,
        ) {
            Ok(request) => request,
            Err(failure) => fail!(position, failure.to_string()),
        };
        let pow_reply = match worker
            .call(module_fn_id(ModuleFunction::SolvePow), pow_request)
            .await
        {
            Ok(reply) => reply,
            Err(failure) => fail!(position, failure.message()),
        };
        let pow = match decode_pow_reply(&pow_reply.bytes) {
            Ok(reply) => reply.solutions,
            Err(failure) => fail!(position, failure.to_string()),
        };
        let blobs = batch
            .images
            .iter()
            .map(|image| image.blob.clone())
            .collect();
        let encode_request = match encode_envelope_request(
            nonce.nonce,
            send.send_manifest_bytes.clone(),
            manifest.manifest_bytes.clone(),
            assertion.clone(),
            pow,
            batch.serial.clone(),
            blobs,
        ) {
            Ok(request) => request,
            Err(failure) => fail!(position, failure.to_string()),
        };
        let encode_reply = match worker
            .call(module_fn_id(ModuleFunction::EncodeEnvelope), encode_request)
            .await
        {
            Ok(reply) => reply,
            Err(failure) => fail!(position, failure.message()),
        };
        let envelope = match decode_encode_reply(&encode_reply.bytes) {
            Ok(reply) => reply.envelope,
            Err(failure) => fail!(position, failure.to_string()),
        };
        match webauthn::upload_envelope(envelope).await {
            Ok(reply) => {
                if let Some(token) = &reply.status_token {
                    let _ = idb::put(idb::StorageKey::STATUS_TOKEN, token).await;
                }
                accepted.push((batch.id, reply));
                photos_sent += batch.images.len();
                send_parts.set(Some((photos_sent, total_photos)));
            }
            Err(message) => {
                worker.terminate();
                return SendOutcome {
                    accepted,
                    error: Some((position, message)),
                };
            }
        }
    }
    worker.terminate();
    SendOutcome {
        accepted,
        error: None,
    }
}

fn find_file<'a>(files: &'a [LocalFile], name: &str) -> Option<&'a LocalFile> {
    files.iter().find(|file| file.name() == name)
}

fn performance_now() -> f64 {
    web_sys::window()
        .and_then(|window| window.performance())
        .map(|performance| performance.now())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn draft(card: bool, malformed: bool) -> DraftGroup<'static> {
        DraftGroup {
            body: "fujifilm/x-t5",
            firmware: "1.00",
            sim: "provia",
            controls: "",
            reproduced: &[],
            unmapped: &[],
            snapshot_version: 0,
            recipe: false,
            target: "",
            card,
            tag: None,
            malformed,
            card_firmware_id: "acme-sim",
            card_firmware_version: "2.0.1",
            monochrome: false,
            monochrome_opt_in: false,
        }
    }

    fn summary() -> PrepSummary {
        PrepSummary {
            metadata: looklab_upload::ManifestMetadata {
                make: None,
                model: None,
                firmware: None,
                lens_model: None,
                focal_length_mm: None,
                iso: None,
                shutter_seconds: None,
                aperture_f_number: None,
                exposure_compensation_ev: None,
                as_shot_wb: None,
                capture_month: None,
                software: None,
                render_tag: None,
                render_tag_malformed: false,
                look: None,
            },
            removed: Vec::new(),
            rerender_group: None,
            alignment: looklab_prep_module::AlignmentSummary {
                correlation: 0.0,
                residual_px: 0.0,
            },
            blob_len: 0,
        }
    }

    fn prepared_image() -> PreparedImageView {
        let summary = summary();
        PreparedImageView {
            blob: Vec::new(),
            metadata: summary.metadata,
            removed: Vec::new(),
            rerender_group: None,
            alignment: summary.alignment,
        }
    }

    fn reading(raw: &str, make: &str, model: &str, sim: &str, control: i64) -> PairReading {
        render_reading(
            raw,
            &format!("{raw}.jpg"),
            make,
            model,
            sim,
            control,
            "1.00",
        )
    }

    fn render_reading(
        raw: &str,
        render: &str,
        make: &str,
        model: &str,
        sim: &str,
        control: i64,
        firmware: &str,
    ) -> PairReading {
        PairReading {
            raw: raw.to_owned(),
            render: RenderFile::new(render).expect("render name"),
            camera: Some(CameraKey {
                make: Some(make.to_owned()),
                model: Some(model.to_owned()),
            }),
            body: format!("{make}/{model}"),
            epoch: Some(1),
            firmware: firmware.to_owned(),
            sim: Some(sim.to_owned()),
            controls: vec![("color".to_owned(), control)],
            unavailable: Vec::new(),
            unmapped: Vec::new(),
            card: None,
            malformed: false,
            render_class: RenderClass::Colour,
            family: None,
            mono_filter: None,
            monochrome: Vec::new(),
            note: None,
        }
    }

    #[test]
    fn an_unselected_monochrome_render_is_listed_and_left_out() {
        let pair = looklab_wasm::PairGroup {
            raw: "a.raf".to_owned(),
            jpegs: vec!["a-acros.jpg".to_owned(), "a-provia.jpg".to_owned()],
        };
        let excluded = HashSet::from(["a-acros.jpg".to_owned(), "b-acros.jpg".to_owned()]);
        let (usable, rows) = split_renders(&pair, &excluded);
        assert_eq!(
            usable.iter().map(RenderFile::as_str).collect::<Vec<_>>(),
            vec!["a-provia.jpg"]
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "a-acros.jpg");

        let only_monochrome = looklab_wasm::PairGroup {
            raw: "b.raf".to_owned(),
            jpegs: vec!["b-acros.jpg".to_owned()],
        };
        let (usable, rows) = split_renders(&only_monochrome, &excluded);
        assert!(usable.is_empty());
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn a_selected_monochrome_render_is_prepared() {
        let pair = looklab_wasm::PairGroup {
            raw: "a.raf".to_owned(),
            jpegs: vec!["a-acros.jpg".to_owned(), "a-provia.jpg".to_owned()],
        };
        let (usable, rows) = split_renders(&pair, &HashSet::new());
        assert_eq!(
            usable.iter().map(RenderFile::as_str).collect::<Vec<_>>(),
            vec!["a-acros.jpg", "a-provia.jpg"]
        );
        assert!(rows.is_empty());
    }

    #[test]
    fn excluded_files_group_by_reason() {
        let row = |name: &str, message: &str| FailureRow {
            name: name.to_owned(),
            message: message.to_owned(),
            donated: false,
        };
        let groups = group_failures(vec![
            row("a.jpg", MONOCHROME_UNSELECTED),
            row("b.jpg", "the JPEG file name is invalid"),
            row("c.jpg", MONOCHROME_UNSELECTED),
        ]);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, MONOCHROME_UNSELECTED);
        assert_eq!(
            groups[0]
                .1
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a.jpg", "c.jpg"]
        );
        assert_eq!(groups[1].0, "the JPEG file name is invalid");
    }

    #[test]
    fn a_monochrome_batch_needs_its_opt_in_to_build_settings() {
        let settings = group_settings(DraftGroup {
            sim: "acros",
            monochrome: true,
            monochrome_opt_in: true,
            ..draft(false, false)
        })
        .expect("an opted-in B&W batch builds settings");
        assert_eq!(settings.render_class, RenderClass::Monochrome);
        assert_eq!(
            settings.monochrome_authorization,
            Some(MonochromeAuthorization::ModelDevelopment180Days)
        );

        let colour = group_settings(draft(false, false)).expect("a colour batch builds settings");
        assert_eq!(colour.render_class, RenderClass::Colour);
        assert_eq!(colour.monochrome_authorization, None);

        group_settings(DraftGroup {
            sim: "acros",
            monochrome: true,
            ..draft(false, false)
        })
        .expect_err("an unselected B&W batch cannot be signed");
    }

    #[test]
    fn malformed_tags_block_native_and_recipe_uploads() {
        assert!(group_settings(draft(false, true)).is_err());
    }

    #[test]
    fn malformed_tags_upload_as_a_user_declared_card() {
        let settings = group_settings(draft(true, true)).expect("card declaration is accepted");
        assert!(matches!(
            settings.intent,
            IntentChoice::Card(CardDeclaration::UserDeclared { .. })
        ));
    }

    #[test]
    fn controls_keep_their_source() {
        let reproduced = vec![
            ("color".to_owned(), 0),
            ("color_chrome_effect".to_owned(), 1),
        ];
        let settings = group_settings(DraftGroup {
            controls: "color=0\ncolor_chrome_effect=2\nshadow_tone=1",
            reproduced: &reproduced,
            ..draft(false, false)
        })
        .expect("settings");
        assert_eq!(settings.controls[0].source, SettingSource::Reproduced);
        assert_eq!(
            settings.controls[1].source,
            SettingSource::Declared,
            "an edited step is declared"
        );
        assert_eq!(
            settings.controls[2].source,
            SettingSource::Declared,
            "a control the makernotes did not supply is declared"
        );
    }

    #[test]
    fn a_restored_batch_still_marks_reproduced_controls() {
        let mut reading = reading("a.raf", "fujifilm", "x-s10", "provia", 0);
        reading.controls = vec![("color".to_owned(), 0), ("clarity".to_owned(), 0)];
        let group = ContributionGroup::from_reading(BatchId(1), &reading);
        let json = serde_json::to_string(&group).expect("json");
        let restored: ContributionGroup = serde_json::from_str(&json).expect("restore");
        let settings = group_settings_for(&restored, &[], 0).expect("settings");
        assert_eq!(settings.controls[0].source, SettingSource::Reproduced);
        assert_eq!(settings.controls[1].source, SettingSource::Reproduced);
    }

    #[test]
    fn an_unmapped_look_affecting_control_blocks_the_batch() {
        let unmapped = vec!["color_chrome_fx_blue".to_owned()];
        group_settings(DraftGroup {
            controls: "color=0",
            unmapped: &unmapped,
            ..draft(false, false)
        })
        .expect_err("the batch is blocked");
        let settings = group_settings(DraftGroup {
            controls: "color=0\ncolor_chrome_fx_blue=0",
            unmapped: &unmapped,
            ..draft(false, false)
        })
        .expect("a declared value unblocks it");
        assert_eq!(
            settings.controls[1].source,
            SettingSource::Declared,
            "{:?}",
            settings.controls[1]
        );
    }

    #[test]
    fn mixed_cameras_looks_and_settings_get_their_own_batch() {
        let readings = vec![
            reading("a.raf", "fujifilm", "x-t5", "provia", 0),
            reading("b.raf", "fujifilm", "x-t5", "provia", 0),
            reading("c.raf", "fujifilm", "x-t5", "velvia", 0),
            reading("d.raf", "sony", "a7", "provia", 0),
            reading("e.raf", "fujifilm", "x-t5", "provia", 1),
        ];
        let set = BatchSet::from_readings(&readings);
        assert_eq!(set.groups().len(), 4);
        assert_eq!(
            set.groups()[0].raws,
            vec!["a.raf".to_owned(), "b.raf".to_owned()]
        );
        assert_eq!(set.groups()[0].fields.body, "fujifilm/x-t5");
        assert_eq!(set.groups()[0].fields.sim, "provia");
        assert_eq!(set.groups()[0].fields.controls, "color=0");
        assert_eq!(set.groups()[1].fields.sim, "velvia");
        assert_eq!(set.groups()[1].fields.controls, "color=0");
        assert_eq!(set.groups()[2].fields.body, "sony/a7");
        assert_eq!(set.groups()[3].fields.controls, "color=1");
        assert!(set.groups().iter().all(|group| !group.serial_opt_in));
    }

    #[test]
    fn a_shared_raw_batches_by_each_renders_sim_settings_and_firmware() {
        let readings = vec![
            render_reading(
                "DSCF0100.RAF",
                "DSCF0100.JPG",
                "fujifilm",
                "x-t4",
                "provia",
                0,
                "1.00",
            ),
            render_reading(
                "DSCF0100.RAF",
                "RENDER-VELVIA.JPG",
                "fujifilm",
                "x-t4",
                "velvia",
                0,
                "1.00",
            ),
            render_reading(
                "DSCF0100.RAF",
                "RENDER-PUNCHY.JPG",
                "fujifilm",
                "x-t4",
                "provia",
                2,
                "1.00",
            ),
            render_reading(
                "DSCF0100.RAF",
                "RENDER-FIRMWARE.JPG",
                "fujifilm",
                "x-t4",
                "provia",
                0,
                "1.10",
            ),
        ];
        let set = BatchSet::from_readings(&readings);

        assert_eq!(set.groups().len(), 4);
        for group in set.groups() {
            assert_eq!(group.raws, vec!["DSCF0100.RAF".to_owned()]);
            assert_eq!(group.renders.len(), 1);
        }
        assert_eq!(
            set.groups()
                .iter()
                .map(|group| group.fields.sim.as_str())
                .collect::<Vec<_>>(),
            vec!["provia", "velvia", "provia", "provia"]
        );
        assert_eq!(set.groups()[1].renders[0].as_str(), "RENDER-VELVIA.JPG");
        assert_eq!(set.groups()[2].fields.controls, "color=2");
        assert_eq!(set.groups()[3].fields.firmware, "1.10");
    }

    #[test]
    fn a_shared_raws_monochrome_render_forms_its_own_batch() {
        let mut acros = render_reading(
            "DSCF0101.RAF",
            "RENDER-ACROS.JPG",
            "fujifilm",
            "x-t4",
            "acros",
            0,
            "1.10",
        );
        acros.monochrome = vec![looklab_wasm::MonochromeRender {
            file: "RENDER-ACROS.JPG".to_owned(),
            sim: "acros".to_owned(),
        }];
        let readings = vec![
            render_reading(
                "DSCF0101.RAF",
                "DSCF0101.JPG",
                "fujifilm",
                "x-t4",
                "provia",
                0,
                "1.10",
            ),
            render_reading(
                "DSCF0101.RAF",
                "RENDER-VELVIA.JPG",
                "fujifilm",
                "x-t4",
                "velvia",
                0,
                "1.10",
            ),
            acros,
        ];
        let set = BatchSet::from_readings(&readings);
        let pair = looklab_wasm::PairGroup {
            raw: "DSCF0101.RAF".to_owned(),
            jpegs: vec![
                "DSCF0101.JPG".to_owned(),
                "RENDER-VELVIA.JPG".to_owned(),
                "RENDER-ACROS.JPG".to_owned(),
            ],
        };
        let (usable, rows) = split_renders(&pair, &HashSet::from(["RENDER-ACROS.JPG".to_owned()]));

        assert_eq!(
            usable.iter().map(RenderFile::as_str).collect::<Vec<_>>(),
            vec!["DSCF0101.JPG", "RENDER-VELVIA.JPG"]
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "RENDER-ACROS.JPG");
        assert_eq!(set.groups().len(), 3);
        let acros_batch = set
            .groups()
            .iter()
            .find(|group| group.fields.sim == "acros")
            .expect("the B&W render has its own batch");
        assert!(acros_batch.is_monochrome());
        assert_eq!(acros_batch.monochrome.len(), 1);
        assert!(
            set.groups()
                .iter()
                .filter(|group| group.fields.sim != "acros")
                .all(|group| !group.is_monochrome())
        );
    }

    #[test]
    fn a_selected_bw_batch_counts_its_deferred_renders() {
        let mut acros = render_reading(
            "DSCF0101.RAF",
            "RENDER-ACROS.JPG",
            "fujifilm",
            "x-t4",
            "acros",
            0,
            "1.10",
        );
        acros.monochrome = vec![looklab_wasm::MonochromeRender {
            file: "RENDER-ACROS.JPG".to_owned(),
            sim: "acros".to_owned(),
        }];
        let colour = render_reading(
            "DSCF0101.RAF",
            "DSCF0101.JPG",
            "fujifilm",
            "x-t4",
            "provia",
            0,
            "1.10",
        );
        let mut set = BatchSet::from_readings(&[colour, acros]);
        let acros_id = set
            .groups()
            .iter()
            .find(|group| group.fields.sim == "acros")
            .expect("B&W batch")
            .id;
        set.update(acros_id, |group| group.fields.monochrome_opt_in = true);
        let pending = set
            .groups()
            .iter()
            .map(|group| PendingBatch {
                id: group.id,
                settings: group_settings_for(group, &[], 0).expect("batch settings"),
                images: vec![prepared_image()],
                serial: None,
            })
            .collect::<Vec<_>>();
        let sent = sent_summary_of(&pending);
        assert_eq!(sent.renders, 2);
        assert_eq!(sent.deferred, 1);
        assert_eq!(sent.cameras, 1);
    }

    #[test]
    fn serial_opt_in_starts_off_for_every_batch() {
        let mut set = BatchSet::from_readings(&[reading("a.raf", "fujifilm", "x-t5", "provia", 0)]);
        let id = set.groups()[0].id;
        set.set_serial(id, true);
        assert!(set.get(id).is_some_and(|group| group.serial_opt_in));
        set.finish();
        assert!(set.groups().is_empty());
        let next = BatchSet::from_readings(&[reading("a.raf", "fujifilm", "x-t5", "provia", 0)]);
        assert!(next.groups().iter().all(|group| !group.serial_opt_in));
    }

    #[test]
    fn a_stale_prefill_answer_is_dropped() {
        let mut ledger = PrefillLedger::default();
        let epoch = ledger.reset();
        let first = ledger.begin(epoch, "a.raf");
        let second = ledger.begin(epoch, "a.raf");
        assert!(!ledger.accept(&first), "a newer request supersedes it");
        assert!(ledger.accept(&second));
        let other = ledger.begin(epoch, "b.raf");
        assert!(ledger.accept(&other), "another file is unaffected");

        let stale = ledger.begin(epoch, "c.raf");
        let epoch = ledger.reset();
        assert!(!ledger.accept(&stale), "a new drop invalidates it");
        let old_epoch = ledger.begin(epoch - 1, "a.raf");
        assert!(!ledger.accept(&old_epoch));
        let fresh = ledger.begin(epoch, "a.raf");
        assert!(ledger.accept(&fresh), "the new drop still owns the file");
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let mut future = Box::pin(future);
        let mut context = Context::from_waker(std::task::Waker::noop());
        for _ in 0..10_000 {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
        panic!("the test future did not finish");
    }

    struct YieldOnce(bool);

    impl Future for YieldOnce {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
            if self.0 {
                Poll::Ready(())
            } else {
                self.0 = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    type LaneFutures = Vec<Pin<Box<dyn Future<Output = Vec<(usize, usize)>>>>>;

    #[test]
    fn the_pool_runs_lanes_concurrently_and_keeps_their_order() {
        let in_flight = Rc::new(RefCell::new((0_usize, 0_usize)));
        let mut lanes: LaneFutures = Vec::new();
        for lane in 0..3 {
            let counter = Rc::clone(&in_flight);
            lanes.push(Box::pin(async move {
                let mut out = Vec::new();
                for item in 0..2 {
                    {
                        let mut state = counter.borrow_mut();
                        state.0 += 1;
                        state.1 = state.1.max(state.0);
                    }
                    YieldOnce(false).await;
                    counter.borrow_mut().0 -= 1;
                    out.push((lane, item));
                }
                out
            }));
        }
        let collected = block_on(join_all(lanes));
        assert_eq!(in_flight.borrow().1, 3, "all three lanes were in flight");
        assert_eq!(
            collected,
            vec![
                vec![(0, 0), (0, 1)],
                vec![(1, 0), (1, 1)],
                vec![(2, 0), (2, 1)],
            ]
        );
    }
}
