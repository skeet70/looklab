// SPDX-License-Identifier: AGPL-3.0-or-later
//! Local contribution flow and payload inspector ([docs/08 §4](../../../docs/08-ui.md)).

use leptos::html::Canvas;
use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_prep::LocalImageKey;
use looklab_upload::{CardDeclaration, ContributionKind, FirmwareId, FirmwareVersion, RenderTag};
use looklab_wasm::coverage::{CoverageMap, coverage_map, sample_points};
use looklab_wasm::prep::{ModuleFunction, default_pow};
use looklab_wasm::{
    CardCheckError, GroupSettings, IntentChoice, PairingPlan, PowParamsSpec, PrepSummary,
    PreparedImageView, SettingsPrefill, build_manifest_request, check_card,
    decode_build_manifest_reply, decode_encode_reply, decode_pow_reply, decode_prepare_reply,
    encode_envelope_request, module_fn_id, prefill_settings, prepare_request,
    serial_ascii_for_file, solve_pow_request,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

use crate::data;
use crate::files::{
    ContributionDraftStore, DraftFile, LocalFile, download, draft_file_entries, read_draft_file,
    read_files,
};
use crate::ui::{Disclosure, DropZone, StepIndex, Stepper, WizardStep};
use crate::worker::{PrepWorker, default_worker_count, max_worker_count};
use crate::{idb, webauthn};

const PRIOR_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct ContributionDraft {
    files: Vec<DraftFile>,
    camera: String,
    firmware: String,
    look: String,
    controls: String,
    recipe: bool,
    target: String,
    custom_firmware: bool,
    firmware_id: String,
    firmware_version: String,
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

#[wasm_bindgen::prelude::wasm_bindgen(
    inline_js = "export async function contributePlatformStore() { return !!globalThis.PublicKeyCredential && await PublicKeyCredential.isUserVerifyingPlatformAuthenticatorAvailable(); }"
)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(catch, js_name = contributePlatformStore)]
    async fn platform_store() -> Result<wasm_bindgen::JsValue, wasm_bindgen::JsValue>;
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessedGroup {
    pub(crate) raw: String,
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

#[component]
pub fn ContributePage() -> impl IntoView {
    let session_context = crate::session::use_session();
    let stage = RwSignal::new(Stage::Drop);
    let current = RwSignal::new(StepIndex(0));
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
    let all_files = RwSignal::new(Vec::<LocalFile>::new());
    let body = RwSignal::new("fujifilm/x-s10".to_owned());
    let firmware = RwSignal::new("1.10".to_owned());
    let sim = RwSignal::new("provia".to_owned());
    let controls = RwSignal::new(String::new());
    let recipe_target = RwSignal::new(String::new());
    let intent_recipe = RwSignal::new(false);
    let intent_card = RwSignal::new(false);
    let card_firmware_id = RwSignal::new(String::new());
    let card_firmware_version = RwSignal::new(String::new());
    let card_firmware_ids = RwSignal::new(Vec::<String>::new());
    let malformed_tag = RwSignal::new(false);
    let worker_count = RwSignal::new(default_worker_count());
    let processed = RwSignal::new(Vec::<ProcessedGroup>::new());
    let seeds = RwSignal::new(Vec::<SeedRow>::new());
    let coverage = RwSignal::new(None::<CoverageMap>);
    let points = RwSignal::new(Vec::<[f32; 3]>::new());
    let failures = RwSignal::new(Vec::<FailureRow>::new());
    let busy = RwSignal::new(false);
    let message = RwSignal::new(None::<String>);
    let status_link = RwSignal::new(None::<String>);
    let serial_opt_in = RwSignal::new(false);
    let serial_present = RwSignal::new(false);
    let prefill_note = RwSignal::new(None::<String>);
    let available = crate::base::server_available();
    let no_platform_store = RwSignal::new(false);
    let draft_loaded = RwSignal::new(false);
    let draft_complete = RwSignal::new(false);
    let draft_files = RwSignal::new(Vec::<DraftFile>::new());
    let draft_store = StoredValue::new_local(ContributionDraftStore::new());
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
            let entries = files
                .iter()
                .map(|file| (file.name.clone(), file.bytes.clone()))
                .collect::<Vec<_>>();
            if let Ok(pairing) = looklab_wasm::inspect_files(&entries)
                && !pairing.groups.is_empty()
            {
                serial_present.set(pairing.facts.iter().any(|fact| fact.has_serial));
                malformed_tag.set(pairing.facts.iter().any(|fact| fact.render_tag_malformed));
                all_files.set(files);
                plan.set(Some(pairing));
                body.set(draft.camera);
                firmware.set(draft.firmware);
                sim.set(draft.look);
                controls.set(draft.controls);
                intent_recipe.set(draft.recipe);
                recipe_target.set(draft.target);
                intent_card.set(draft.custom_firmware);
                card_firmware_id.set(draft.firmware_id);
                card_firmware_version.set(draft.firmware_version);
                draft_files.set(draft.files);
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
                    .map(|group| ProcessedGroup {
                        raw: group.raw,
                        images: looklab_wasm::prep::summarize(&group.images),
                        blobs: group.images.into_iter().map(|image| image.blob).collect(),
                        bytes_in: group.bytes_in,
                        millis: group.millis,
                        memory: group.memory,
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
            camera: body.get(),
            firmware: firmware.get(),
            look: sim.get(),
            controls: controls.get(),
            recipe: intent_recipe.get(),
            target: recipe_target.get(),
            custom_firmware: intent_card.get(),
            firmware_id: card_firmware_id.get(),
            firmware_version: card_firmware_version.get(),
        };
        if let Ok(text) = serde_json::to_string(&draft) {
            draft_store.with_value(|store| store.schedule(&text));
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

    let on_files = Callback::new(move |selected: Vec<web_sys::File>| {
        busy.set(true);
        spawn_local(async move {
            match read_files(selected).await {
                Ok(files) => {
                    let entries: Vec<(String, Vec<u8>)> = files
                        .iter()
                        .map(|file| (file.name.clone(), file.bytes.clone()))
                        .collect();
                    match looklab_wasm::inspect_files(&entries) {
                        Ok(pairing) => {
                            processed.set(Vec::new());
                            seeds.set(Vec::new());
                            failures.set(Vec::new());
                            coverage.set(None);
                            points.set(Vec::new());
                            draft_complete.set(false);
                            let body_key = pairing.facts.iter().find_map(|fact| {
                                Some((
                                    fact.make.clone()?,
                                    fact.model.clone()?,
                                    fact.firmware.clone()?,
                                ))
                            });
                            match draft_file_entries(&files) {
                                Ok((references, entries)) => {
                                    let pending = draft_store
                                        .with_value(|store| store.replace_files(entries.as_ref()));
                                    let _ = wasm_bindgen_futures::JsFuture::from(pending).await;
                                    draft_files.set(references);
                                }
                                Err(error) => message.set(Some(error)),
                            }
                            all_files.set(files);
                            serial_present.set(pairing.facts.iter().any(|fact| fact.has_serial));
                            let mut ids = Vec::new();
                            let mut first_tag = None;
                            for fact in &pairing.facts {
                                if let Some(tag) = &fact.render_tag {
                                    let id = tag.firmware_id.as_str().to_owned();
                                    if !ids.contains(&id) {
                                        ids.push(id);
                                    }
                                    first_tag.get_or_insert(tag);
                                }
                            }
                            if let Some(tag) = first_tag {
                                intent_card.set(true);
                                intent_recipe.set(false);
                                card_firmware_id.set(tag.firmware_id.as_str().to_owned());
                                card_firmware_version.set(tag.firmware_version.as_str().to_owned());
                            } else {
                                intent_card.set(false);
                                card_firmware_id.set(String::new());
                                card_firmware_version.set(String::new());
                            }
                            card_firmware_ids.set(ids);
                            malformed_tag
                                .set(pairing.facts.iter().any(|fact| fact.render_tag_malformed));
                            plan.set(Some(pairing));
                            current.set(StepIndex(1));
                            prefill_note.set(None);
                            if let Some((make, model, _)) = body_key {
                                match prefill_from_site(&make, &model, &entries).await {
                                    Ok((body_id, prefill)) => {
                                        body.set(body_id);
                                        firmware.set(prefill.firmware);
                                        if let Some(chosen) = prefill.sim {
                                            sim.set(chosen);
                                        }
                                        controls.set(
                                            prefill
                                                .controls
                                                .iter()
                                                .map(|(key, step)| format!("{key}={step}"))
                                                .collect::<Vec<_>>()
                                                .join("\n"),
                                        );
                                        if !prefill.unavailable.is_empty() {
                                            prefill_note.set(Some(format!(
                                                "Please check these settings, which could not be read: {}",
                                                prefill.unavailable.join("; ")
                                            )));
                                        }
                                    }
                                    Err(error) => prefill_note.set(Some(error)),
                                }
                            }
                        }
                        Err(error) => message.set(Some(error.to_string())),
                    }
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    });

    let process = move |_| {
        let Some(pairing) = plan.get() else {
            return;
        };
        let files = all_files.get();
        let count = worker_count.get().max(1);
        busy.set(true);
        processed.set(Vec::new());
        current.set(StepIndex(2));
        message.set(Some(
            "Processing locally. Nothing leaves this page yet.".to_owned(),
        ));
        spawn_local(async move {
            let result = run_files_progress(
                &pairing,
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
                        let _ = wasm_bindgen_futures::JsFuture::from(pending).await;
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
        let groups = processed.get();
        if groups.is_empty() || busy.get() || draft_complete.get() {
            return;
        }
        let images = prepared_images(&groups);
        let card_tag = images
            .iter()
            .find_map(|image| image.metadata.render_tag.clone());
        let malformed = images
            .iter()
            .any(|image| image.metadata.render_tag_malformed);
        let settings = match group_settings(DraftGroup {
            body: &body.get(),
            firmware: &firmware.get(),
            sim: &sim.get(),
            controls: &controls.get(),
            recipe: intent_recipe.get(),
            target: &recipe_target.get(),
            card: intent_card.get() || card_tag.is_some(),
            tag: card_tag.as_ref(),
            malformed,
            card_firmware_id: &card_firmware_id.get(),
            card_firmware_version: &card_firmware_version.get(),
        }) {
            Ok(settings) => settings,
            Err(error) => {
                message.set(Some(error));
                return;
            }
        };
        let files = all_files.get();
        let seed_rows = seeds.get();
        let opt_in = serial_opt_in.get();
        busy.set(true);
        message.set(Some(
            "Confirm this batch on your device. Preparing upload (this keeps spam out).".to_owned(),
        ));
        spawn_local(async move {
            match upload_batch(session_context, settings, images, files, seed_rows, opt_in).await {
                Ok(reply) => {
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
                    let link = reply
                        .status_token
                        .as_ref()
                        .map(|token| crate::base::site_url(&format!("/status/{token}")));
                    status_link.set(link);
                    draft_complete.set(true);
                    let pending = draft_store.with_value(ContributionDraftStore::clear);
                    let _ = wasm_bindgen_futures::JsFuture::from(pending).await;
                    processed.set(Vec::new());
                    seeds.set(Vec::new());
                    all_files.set(Vec::new());
                    points.set(Vec::new());
                    current.set(StepIndex(5));
                    message.set(Some(format!(
                        "Uploaded contribution {}.",
                        reply.contribution_id
                    )));
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let download_payload = move |_| {
        let groups = processed.get();
        let payload = serde_json::json!({
            "format": "looklab-payload-view/1",
            "authoritative": false,
            "note": "A convenience view, not a trust anchor. Verify the request body actually sent against the HAR.",
            "body": body.get(),
            "firmware": firmware.get(),
            "sim": sim.get(),
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
        let files = all_files.get();
        let Some(file) = files.iter().find(|file| file.name == name) else {
            return;
        };
        if let Err(error) = download(&file.name, &file.bytes, "application/octet-stream") {
            message.set(Some(error));
            return;
        }
        let mut rows = failures.get();
        if let Some(row) = rows.iter_mut().find(|row| row.name == name) {
            row.donated = true;
        }
        failures.set(rows);
        spawn_local(async move {
            let consent = format!(
                "{{\"file\":\"{}\",\"consent\":\"fixture donation\",\"recorded_at\":{}}}",
                name,
                js_sys::Date::now()
            );
            let _ = idb::put(idb::StorageKey::FIXTURE_CONSENT, &consent).await;
        });
        message.set(Some(
            "Consent saved on this device. Follow the fixture donation instructions when sharing the downloaded file."
                .to_owned(),
        ));
    };

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
                {[
                    ("Share photos you already have", "About a minute", "Medium", "Start with RAW+JPEG pairs from your library."),
                    ("Take a few wanted shots", "5 to 10 minutes", "High", "Fill a gap with colors we need to measure."),
                    ("Re-render a few RAWs in camera", "About 2 minutes", "High", "Try the same photos with other built-in looks."),
                    ("Photograph a pattern on your phone", "About 3 minutes", "Medium", "Add a wider range of saturated colors."),
                    ("Photograph a gradient", "About 2 minutes", "Medium", "Help measure contrast and neutral tint."),
                    ("Photograph a color chart", "About 5 minutes", "High", "Help measure how your camera sees color."),
                ].into_iter().enumerate().map(|(index, (title, time, impact, description))| view! {
                    <article class="help-card">
                        <p class="help-meta">{time} " · " {impact} " impact"</p>
                        <h3>{title}</h3>
                        <p>{description}</p>
                        {(index == 1).then(|| view! {
                            <a href=crate::base::site_url("/contribute/wanted")>
                                "See wanted shots →"
                            </a>
                        })}
                    </article>
                }).collect_view()}
            </div>
            <Disclosure summary="Capture tips".to_owned()>
                <p>"Shoot RAW+JPEG at your camera's lowest native ISO. Avoid motion and turn off lens corrections where possible."</p>
                <p>"For phone patterns: use maximum brightness, turn off adaptive color, fill about 60% of the frame, defocus slightly, and shoot at 0, −1 and −2 exposure compensation."</p>
                <p>"For a gradient: photograph a smooth fade from clipped white to black under one light, with white balance set to that light."</p>
                <p>"For a color chart: use even light without glare, fill 30 to 60% of the frame, and shoot at 0, −1 and −2 exposure compensation."</p>
            </Disclosure>
        </section>
        <Stepper
            current
            steps=[
                ("Add photos", "Local files only"),
                ("Check groups", "Settings checked"),
                ("Process", "Processed on this device"),
                ("Review", "Color samples reviewed"),
                ("Send", "Confirmed and sent"),
                ("Done", "Complete"),
            ].into_iter().map(|(title, summary)| WizardStep {
                title: title.to_owned(),
                summary: Some(summary.to_owned()),
            }).collect()
            summaries=Signal::derive(move || {
                let pairs = plan.with(|plan| plan.as_ref().map(|plan| plan.groups.len()).unwrap_or(0));
                vec![
                    Some(format!("{pairs} {}", if pairs == 1 { "pair" } else { "pairs" })),
                    Some(format!("{pairs} {}", if pairs == 1 { "group" } else { "groups" })),
                    coverage.with(|map| map.as_ref().map(|map| format!("{} {} processed", map.images, if map.images == 1 { "photo" } else { "photos" }))),
                    None,
                    None,
                    None,
                ]
            })
            locked=Signal::derive(move || busy.get() || draft_complete.get())
            can_advance=Signal::derive(move || !busy.get() && matches!(stage.get(), Stage::Inspector))
        />
        {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}
        {move || {
            let rows = failures.get();
            (!rows.is_empty()).then(|| view! {
                <section class="notice" aria-label="Excluded photos">
                    <h3>"Photos not included"</h3>
                    <ul>{rows.into_iter().map(|row| view! {
                        <li><strong>{row.name.clone()}</strong>": This file could not be processed. Try the original RAW+JPEG pair."
                            <Disclosure summary="Technical details".to_owned()><p>{row.message.clone()}</p></Disclosure>
                        </li>
                    }).collect_view()}</ul>
                </section>
            })
        }}

        <section class="step" hidden=move || stage.get() != Stage::Drop>
            <h2>"Drop files or a folder"</h2>
            <DropZone
                on_files
                accept=".raf,.dng,.jpg,.jpeg,.arw,.cr2,.nef,.orf,.rw2".to_owned()
                disabled=Signal::derive(move || busy.get() || !draft_loaded.get())
                prompt="Drop RAW+JPEG files or a folder here. Photos stay on this device, even when you send color samples.".to_owned()
            />
            <p class="muted">"Your files, settings and prepared samples stay in a local draft in this browser. Keep your originals in case browser storage is cleared."</p>
        </section>

        <section class="step" hidden=move || stage.get() != Stage::Confirm>
            <h2>"Check the groups"</h2>
            {move || plan.get().map(|pairing| view! { <Pairing plan=pairing look=sim/> })}
            <p>"Check the camera, built-in look and settings below. If these photos use a recipe to match another look, choose that option and name the target."</p>
            <label for="body-id">"Camera"</label>
            <input id="body-id" type="text" prop:value=move || body.get()
                on:input=move |event| body.set(event_target_value(&event))/>
            <label for="firmware">"Firmware"</label>
            <input id="firmware" type="text" prop:value=move || firmware.get()
                on:input=move |event| firmware.set(event_target_value(&event))/>
            <label for="sim">"Film simulation"</label>
            <input id="sim" type="text" prop:value=move || sim.get()
                on:input=move |event| sim.set(event_target_value(&event))/>
            <label>
                <input type="checkbox" prop:checked=move || intent_recipe.get()
                    on:change=move |event| intent_recipe.set(event_target_checked(&event))/>
                " Recipe contribution"
            </label>
            <div hidden=move || !intent_recipe.get()>
            <label for="recipe-target">"Look this recipe is trying to match"</label>
            <input id="recipe-target" type="text" prop:value=move || recipe_target.get()
                on:input=move |event| recipe_target.set(event_target_value(&event))/>
            </div>
            <label>
                <input type="checkbox" prop:checked=move || intent_card.get()
                    on:change=move |event| intent_card.set(event_target_checked(&event))/>
                " Rendered by custom firmware"
            </label>
            <div hidden=move || !intent_card.get()>
            <p class="muted">
                "The JPEG's custom-firmware label fills this in when available. Otherwise, enter the firmware you used."
            </p>
            <label for="card-firmware-id">"Custom firmware name"</label>
            <input id="card-firmware-id" type="text" list="card-firmware-ids"
                prop:value=move || card_firmware_id.get()
                on:input=move |event| card_firmware_id.set(event_target_value(&event))/>
            <datalist id="card-firmware-ids">
                {move || card_firmware_ids.get().into_iter().map(|id| view! {
                    <option value=id></option>
                }).collect_view()}
            </datalist>
            <label for="card-firmware-version">"Custom firmware version"</label>
            <input id="card-firmware-version" type="text"
                prop:value=move || card_firmware_version.get()
                on:input=move |event| card_firmware_version.set(event_target_value(&event))/>
            </div>
            {move || malformed_tag.get().then(|| view! {
                <p class="warn" role="alert">
                    "Some photos have an unreadable custom-firmware label. Choose custom firmware and enter its name and version, or remove those files."
                </p>
            })}
            <h3>"Camera settings"</h3>
            {move || menu_settings(&controls.get()).into_iter().map(|setting| {
                let key = setting.key;
                let label = key.as_str().replace('_', " ");
                view! {
                    <label class="menu-setting">{label}
                        <input type="number" prop:value=setting.step on:input=move |event| {
                            if let Ok(step) = event_target_value(&event).parse::<i64>() {
                                let mut fields = menu_settings(&controls.get());
                                if let Some(field) = fields.iter_mut().find(|field| field.key == key) { field.step = step; }
                                controls.set(fields.into_iter().map(|field| format!("{}={}", field.key, field.step)).collect::<Vec<_>>().join("\n"));
                            }
                        }/>
                    </label>
                }
            }).collect_view()}
            <Disclosure summary="Technical settings editor".to_owned()>
            <label for="controls">"Settings (key=step, one per line)"</label>
            <textarea id="controls" rows="3" prop:value=move || controls.get()
                on:input=move |event| controls.set(event_target_value(&event))></textarea>
            </Disclosure>
            {move || prefill_note.get().map(|note| view! { <p class="muted" role="status">{note}</p> })}
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
            <progress aria-label="Processing photos"></progress>
            <p>"Extracting color samples locally. Your photos stay on this device."</p>
            <p role="status">{move || format!("{} of {} photo groups processed", processed.get().len(), plan.get().map(|plan| plan.groups.len()).unwrap_or(0))}</p>
            {move || coverage.get().map(|map| view! { <CoverageMapView map/> })}
        </section>
        <section class="step" hidden=move || stage.get() != Stage::Inspector>
            <h2>"Review what's sent"</h2>
            {move || coverage.get().map(|map| view! {
                <p class="payload-summary">{format!("{} color samples from {} {}, {:.0} KB. No pixels, no location.", points.with(Vec::len), map.images, if map.images == 1 { "photo" } else { "photos" }, processed.with(|groups| groups.iter().flat_map(|group| &group.images).map(|image| image.blob_len).sum::<usize>()) as f64 / 1024.0)}</p>
            })}
            {move || coverage.get().map(|map| view! { <CoverageMapView map/> })}
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
                        let jpeg_name = plan.with(|plan| plan.as_ref().and_then(|plan| plan.groups.iter().find(|pair| pair.raw == group.raw)).and_then(|pair| pair.jpegs.get(index)).cloned());
                        let source_size = all_files.with(|files| files.iter().filter(|file| file.name == group.raw || jpeg_name.as_ref() == Some(&file.name)).map(|file| file.bytes.len()).sum::<usize>());
                        view! {
                            <tr class="group-row"><th scope="row" colspan="2">{format!("{} image {}", group.raw, index + 1)}</th></tr>
                            <tr><th scope="row">"Size before → after"</th><td>{format!("{} bytes → {} bytes", source_size, image.blob_len)}</td></tr>
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
            <p>
                "Serial: sent once, converted to a keyed hash on arrival, never stored. "
                <label>
                    <input type="checkbox" prop:checked=move || serial_opt_in.get()
                        on:change=move |event| serial_opt_in.set(event_target_checked(&event))/>
                    {move || if serial_present.get() { " Include my camera serial" } else { " No serial found" }}
                </label>
            </p>
            <p class="muted">
                "Download payload is a convenience view with a JSON rendering, not a trust anchor. To check what was actually sent, export the HAR and run "
                <code>"looklab verify-payload"</code> "."
            </p>
            <button on:click=download_payload>"Download payload (JSON, non-authoritative)"</button>
            <button on:click=download_seeds>"Export seed file (SENSITIVE)"</button>
            <p class="warn">
                "The seed file reveals sampling order. It is never uploaded; keep it local."
            </p>
            <p>
                "Source: "
                <a href="https://github.com/skeet70/looklab">"looklab (AGPL-3.0-or-later)"</a>
                ". Prefer not to trust a served client? Use the CLI, which you can build from source."
            </p>
            {move || {
                let rows = failures.get();
                (!rows.is_empty()).then(|| view! {
                    <div class="error" role="alert">
                        <h3>"Files excluded from the batch"</h3>
                        <ul>
                            {rows.into_iter().map(|row| view! {
                                <li>
                                    <strong>{row.name.clone()}</strong> ": " {row.message.clone()}
                                    {(!row.donated).then(|| view! {
                                        <button on:click={
                                            let name = row.name.clone();
                                            move |_| donation(name.clone())
                                        }>"Donate this file as a test fixture"</button>
                                    })}
                                </li>
                            }).collect_view()}
                        </ul>
                    </div>
                })
            }}
            <h3>"Timing"</h3>
            <table class="data-table">
                <thead><tr><th scope="col">RAW</th><th scope="col">"Size before → after"</th><th scope="col">Time</th><th scope="col">Worker memory</th></tr></thead>
                <tbody>
                    {move || processed.get().into_iter().map(|group| view! {
                        <tr>
                            <th scope="row">{group.raw}</th>
                            <td>{format!("{} bytes → {} bytes", group.bytes_in, group.images.iter().map(|image| image.blob_len).sum::<usize>())}</td>
                            <td>{format!("{:.0} ms", group.millis)}</td>
                            <td>{format!("{:.0} MiB", group.memory as f64 / (1024.0 * 1024.0))}</td>
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
            </Disclosure>
            <button class="button-primary" on:click=move |_| current.set(StepIndex(4))>"Continue to send"</button>
        </section>
        <section class="step" hidden=move || stage.get() != Stage::Send>
            <h2>"Send your color samples"</h2>
            <p>"Sign in or create a profile, then confirm this batch once on your device. You can follow its progress in Your contributions."</p>
            {(!available).then(crate::ui::preview_notice)}
            <button class="button-primary" on:click=upload disabled=move || busy.get() || !available>"Send contribution"</button>
        </section>
        <section class="step contribution-done" hidden=move || stage.get() != Stage::Done>
            <h2>"Thanks for helping build looks"</h2>
            <p>"Your samples are sent. Feedback arrives in Your contributions after processing."</p>
            <a class="button" href=crate::base::site_url("/me")>"Your contributions"</a>
            <a href=crate::base::site_url("/contribute/wanted")>
                "Next way to help: take a few wanted shots"
            </a>
            {move || status_link.get().map(|link| view! { <p>"Status link: " <a href=link.clone()>{link.clone()}</a></p> })}
        </section>
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
    let (Some(make), Some(model)) = (&fact.make, &fact.model) else {
        return None;
    };
    index.bodies.iter().find(|camera| {
        camera.make.trim().eq_ignore_ascii_case(make.trim())
            && camera.model.trim().eq_ignore_ascii_case(model.trim())
    })
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
fn Pairing(plan: PairingPlan, look: RwSignal<String>) -> impl IntoView {
    let PairingPlan {
        groups,
        unpaired,
        facts,
    } = plan;
    let pair_count = groups.len();
    let unpaired_count = unpaired.len();
    let catalog = RwSignal::new(None::<GroupCatalog>);
    let camera_facts = facts.clone();
    spawn_local(async move {
        if let Ok(index) = data::index().await {
            let mut schemas = Vec::new();
            for camera in &index.bodies {
                if camera_facts.iter().any(|fact| {
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
    view! {
        <div class="pairing">
            <h3>{format!("{pair_count} {} found", if pair_count == 1 { "pair" } else { "pairs" })}</h3>
            <div class="pairing-cards">
                {groups.into_iter().map(|group| {
                    let fact = facts.iter().find(|fact| fact.name == group.raw).cloned();
                    let camera_fact = fact.clone();
                    view! {
                    <article class="pairing-card">
                        <h4>{move || catalog.with(|catalog| camera_display(camera_fact.as_ref(), catalog.as_ref()))}</h4>
                        <p class="help-meta">{move || {
                            let value = look.get();
                            let name = looklab_schema::SimKey::new(&value).map(|key| catalog.with(|catalog| sim_menu_label(&key, fact.as_ref(), catalog.as_ref()))).unwrap_or_else(|_| title_case_words(&value));
                            format!("Built-in look: {name}")
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

async fn prefill_from_site(
    make: &str,
    model: &str,
    files: &[(String, Vec<u8>)],
) -> Result<(String, SettingsPrefill), String> {
    let index = data::index().await?;
    let summary = index
        .bodies
        .iter()
        .find(|body| body.make.eq_ignore_ascii_case(make) && body.model.eq_ignore_ascii_case(model))
        .ok_or_else(|| format!("the site data has no camera for {make} {model}"))?;
    let page = data::body(summary.id.make(), summary.id.model()).await?;
    let prefill = prefill_settings(&page.schema, files).map_err(|error| error.to_string())?;
    Ok((page.id.to_string(), prefill))
}

struct DraftGroup<'a> {
    body: &'a str,
    firmware: &'a str,
    sim: &'a str,
    controls: &'a str,
    recipe: bool,
    target: &'a str,
    card: bool,
    tag: Option<&'a RenderTag>,
    malformed: bool,
    card_firmware_id: &'a str,
    card_firmware_version: &'a str,
}

fn group_settings(draft: DraftGroup<'_>) -> Result<GroupSettings, String> {
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
        entries.push((
            key.trim().to_owned(),
            step.trim()
                .parse::<i64>()
                .map_err(|_| format!("{line}: step is not an integer"))?,
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
        controls: entries,
    })
}

fn prepared_images(groups: &[ProcessedGroup]) -> Vec<PreparedImageView> {
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

pub(crate) async fn run_files(
    pairing: &PairingPlan,
    files: &[LocalFile],
    count: u32,
) -> Result<(Vec<ProcessedGroup>, Vec<SeedRow>, Vec<FailureRow>), String> {
    run_files_progress(pairing, files, count, None).await
}

async fn run_files_progress(
    pairing: &PairingPlan,
    files: &[LocalFile],
    count: u32,
    progress: Option<Callback<ProcessedGroup>>,
) -> Result<(Vec<ProcessedGroup>, Vec<SeedRow>, Vec<FailureRow>), String> {
    let mut workers = Vec::with_capacity(count as usize);
    for _ in 0..count {
        workers.push(PrepWorker::spawn()?);
    }
    let mut groups = Vec::new();
    let mut seed_rows = Vec::new();
    let mut failures = Vec::new();
    for (index, group) in pairing.groups.iter().enumerate() {
        let slot = index % workers.len();
        if workers[slot].should_recycle() {
            workers[slot].terminate();
            workers[slot] = PrepWorker::spawn()?;
        }
        let Some(raw) = find_file(files, &group.raw) else {
            failures.push(FailureRow {
                name: group.raw.clone(),
                message: "the file is no longer available".to_owned(),
                donated: false,
            });
            continue;
        };
        let mut jpegs = Vec::with_capacity(group.jpegs.len());
        let mut missing = None;
        for name in &group.jpegs {
            match find_file(files, name) {
                Some(file) => jpegs.push(file.bytes.clone()),
                None => missing = Some(name.clone()),
            }
        }
        if let Some(name) = missing {
            failures.push(FailureRow {
                name,
                message: "the file is no longer available".to_owned(),
                donated: false,
            });
            continue;
        }
        if jpegs.is_empty() {
            failures.push(FailureRow {
                name: group.raw.clone(),
                message: "no JPEG is paired with this RAW".to_owned(),
                donated: false,
            });
            continue;
        }
        let mut seed = [0u8; 32];
        web_sys::window()
            .and_then(|window| window.crypto().ok())
            .ok_or_else(|| "crypto.getRandomValues is unavailable".to_owned())?
            .get_random_values_with_u8_array(&mut seed)
            .map_err(|error| format!("{error:?}"))?;
        let request = prepare_request(raw.bytes.clone(), jpegs.clone(), PRIOR_VERSION, seed);
        let bytes_in =
            raw.bytes.len() as u64 + jpegs.iter().map(|jpeg| jpeg.len() as u64).sum::<u64>();
        let start = performance_now();
        let reply = workers[slot]
            .call(module_fn_id(ModuleFunction::Prepare), request)
            .await;
        let millis = performance_now() - start;
        match reply {
            Ok(reply) => match decode_prepare_reply(&reply.bytes) {
                Ok(response) => {
                    seed_rows.push(SeedRow {
                        name: group.raw.clone(),
                        seed,
                    });
                    groups.push(ProcessedGroup {
                        raw: group.raw.clone(),
                        images: looklab_wasm::prep::summarize(&response.images),
                        blobs: response
                            .images
                            .iter()
                            .map(|image| image.blob.clone())
                            .collect(),
                        bytes_in,
                        millis,
                        memory: reply.memory,
                    });
                    if let Some(progress) = progress
                        && let Some(group) = groups.last()
                    {
                        progress.run(group.clone());
                    }
                }
                Err(error) => failures.push(FailureRow {
                    name: group.raw.clone(),
                    message: error.to_string(),
                    donated: false,
                }),
            },
            Err(error) => failures.push(FailureRow {
                name: group.raw.clone(),
                message: error.message(),
                donated: false,
            }),
        }
    }
    let mut card_failures = check_card_tags(&mut groups).await;
    failures.append(&mut card_failures);
    for worker in &workers {
        worker.terminate();
    }
    Ok((groups, seed_rows, failures))
}

async fn check_card_tags(groups: &mut [ProcessedGroup]) -> Vec<FailureRow> {
    let expected = groups
        .iter()
        .flat_map(|group| group.images.iter())
        .find_map(|image| image.metadata.render_tag.clone());
    let mut failures = Vec::new();
    for group in groups {
        let mut drop = Vec::new();
        for (index, image) in group.images.iter().enumerate() {
            if image.metadata.render_tag_malformed {
                if expected.is_some() {
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
            if expected.as_ref().is_some_and(|first| first != tag) {
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
            failures.push(FailureRow {
                name: format!("{} image {}", group.raw, index + 1),
                message: format!("excluded: {reason}"),
                donated: false,
            });
        }
    }
    failures
}

async fn upload_batch(
    session: crate::session::SessionContext,
    settings: GroupSettings,
    images: Vec<PreparedImageView>,
    files: Vec<LocalFile>,
    seeds: Vec<SeedRow>,
    serial_opt_in: bool,
) -> Result<webauthn::UploadReply, String> {
    if images.is_empty() {
        return Err("nothing to upload".to_owned());
    }
    session
        .require_profile()
        .await
        .ok_or_else(|| "Sign in was cancelled. Your prepared samples are still here.".to_owned())?;
    let credential_id = idb::get(idb::StorageKey::CREDENTIAL)
        .await?
        .and_then(|text| webauthn::decode_base64url(&text));
    let nonce = webauthn::fetch_nonce().await?;
    let mut worker = PrepWorker::spawn()?;
    let manifest_request = build_manifest_request(&settings, &images, nonce.nonce)
        .map_err(|error| error.to_string())?;
    let manifest_reply = worker
        .call(
            module_fn_id(ModuleFunction::BuildManifest),
            manifest_request,
        )
        .await
        .map_err(|error| error.message())?;
    let manifest =
        decode_build_manifest_reply(&manifest_reply.bytes).map_err(|error| error.to_string())?;
    let challenge = *manifest.challenge.as_bytes();
    let assertion = webauthn::sign_batch(&challenge, credential_id.as_deref()).await?;
    let defaults = default_pow();
    let pow = nonce.pow.unwrap_or(webauthn::PowParameters {
        memory_kib: defaults.memory_kib,
        iterations: defaults.iterations,
        difficulty_bits_per_image: defaults.difficulty_bits_per_image,
    });
    let n_images = u32::try_from(images.len()).map_err(|_| "too many images".to_owned())?;
    let pow_request = solve_pow_request(
        challenge,
        n_images,
        PowParamsSpec {
            memory_kib: pow.memory_kib,
            iterations: pow.iterations,
            difficulty_bits_per_image: pow.difficulty_bits_per_image,
        },
        0,
    )
    .map_err(|error| error.to_string())?;
    let pow_reply = worker
        .call(module_fn_id(ModuleFunction::SolvePow), pow_request)
        .await
        .map_err(|error| error.message())?;
    let pow = decode_pow_reply(&pow_reply.bytes)
        .map_err(|error| error.to_string())?
        .solutions;
    let serial = if serial_opt_in {
        seeds.first().and_then(|seed| {
            files
                .iter()
                .find(|file| file.name == seed.name)
                .and_then(|file| serial_ascii_for_file(&file.bytes))
        })
    } else {
        None
    };
    let blobs = images.iter().map(|image| image.blob.clone()).collect();
    let encode_request = encode_envelope_request(
        nonce.nonce,
        manifest.manifest_bytes.clone(),
        assertion,
        pow,
        serial,
        blobs,
    )
    .map_err(|error| error.to_string())?;
    let encode_reply = worker
        .call(module_fn_id(ModuleFunction::EncodeEnvelope), encode_request)
        .await
        .map_err(|error| error.message())?;
    let envelope = decode_encode_reply(&encode_reply.bytes)
        .map_err(|error| error.to_string())?
        .envelope;
    worker.terminate();
    let reply = webauthn::upload_envelope(envelope).await?;
    if let Some(token) = &reply.status_token {
        let _ = idb::put(idb::StorageKey::STATUS_TOKEN, token).await;
    }
    Ok(reply)
}

fn find_file<'a>(files: &'a [LocalFile], name: &str) -> Option<&'a LocalFile> {
    files.iter().find(|file| file.name == name)
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

    fn draft(card: bool, malformed: bool) -> DraftGroup<'static> {
        DraftGroup {
            body: "fujifilm/x-t5",
            firmware: "1.00",
            sim: "provia",
            controls: "",
            recipe: false,
            target: "",
            card,
            tag: None,
            malformed,
            card_firmware_id: "acme-sim",
            card_firmware_version: "2.0.1",
        }
    }

    #[test]
    fn malformed_tags_block_native_and_recipe_uploads() {
        let error = group_settings(draft(false, true)).expect_err("native is blocked");
        assert!(error.contains("malformed card render tag"), "{error}");
    }

    #[test]
    fn malformed_tags_upload_as_a_user_declared_card() {
        let settings = group_settings(draft(true, true)).expect("card declaration is accepted");
        assert!(matches!(
            settings.intent,
            IntentChoice::Card(CardDeclaration::UserDeclared { .. })
        ));
    }
}
