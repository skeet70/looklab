// SPDX-License-Identifier: AGPL-3.0-or-later
//! The contribution ladder and payload inspector ([docs/08 §4](../../../docs/08-ui.md)).

use leptos::html::Canvas;
use leptos::prelude::*;
use leptos::task::spawn_local;
use looklab_prep::LocalImageKey;
use looklab_upload::ContributionKind;
use looklab_wasm::coverage::{CoverageMap, coverage_map, sample_points};
use looklab_wasm::prep::{ModuleFunction, default_pow};
use looklab_wasm::{
    GroupSettings, IntentChoice, PairingPlan, PowParamsSpec, PrepSummary, PreparedImageView,
    build_manifest_request, decode_build_manifest_reply, decode_encode_reply, decode_pow_reply,
    decode_prepare_reply, encode_envelope_request, module_fn_id, prepare_request,
    serial_ascii_for_file, solve_pow_request,
};
use wasm_bindgen::JsCast;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, HtmlInputElement};

use crate::files::{LocalFile, download, read_input};
use crate::worker::{PrepWorker, default_worker_count, max_worker_count};
use crate::{idb, webauthn};

const PRIOR_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProcessedGroup {
    pub(crate) raw: String,
    pub(crate) images: Vec<PrepSummary>,
    pub(crate) blobs: Vec<Vec<u8>>,
    pub(crate) bytes_in: u64,
    pub(crate) millis: f64,
    pub(crate) memory: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeedRow {
    pub(crate) name: String,
    pub(crate) seed: [u8; 32],
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FailureRow {
    pub(crate) name: String,
    pub(crate) message: String,
    pub(crate) donated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Drop,
    Confirm,
    Inspector,
}

#[component]
pub fn ContributePage() -> impl IntoView {
    let stage = RwSignal::new(Stage::Drop);
    let plan = RwSignal::new(None::<PairingPlan>);
    let all_files = RwSignal::new(Vec::<LocalFile>::new());
    let body = RwSignal::new("fujifilm/x-s10".to_owned());
    let firmware = RwSignal::new("1.10".to_owned());
    let sim = RwSignal::new("provia".to_owned());
    let controls = RwSignal::new(String::new());
    let recipe_target = RwSignal::new(String::new());
    let intent_recipe = RwSignal::new(false);
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

    let on_files = move |event: leptos::ev::Event| {
        let Some(input) = event
            .target()
            .and_then(|target| target.dyn_into::<HtmlInputElement>().ok())
        else {
            return;
        };
        spawn_local(async move {
            match read_input(&input).await {
                Ok(files) => match looklab_wasm::inspect_files(
                    &files
                        .iter()
                        .map(|file| (file.name.clone(), file.bytes.clone()))
                        .collect::<Vec<_>>(),
                ) {
                    Ok(pairing) => {
                        all_files.set(files);
                        serial_present.set(pairing.facts.iter().any(|fact| fact.has_serial));
                        plan.set(Some(pairing));
                        stage.set(Stage::Confirm);
                    }
                    Err(error) => message.set(Some(error.to_string())),
                },
                Err(error) => message.set(Some(error)),
            }
        });
    };

    let process = move |_| {
        let Some(pairing) = plan.get() else {
            return;
        };
        let files = all_files.get();
        let count = worker_count.get().max(1);
        busy.set(true);
        message.set(Some(
            "Processing locally. Nothing leaves this page yet.".to_owned(),
        ));
        spawn_local(async move {
            let result = run_files(&pairing, &files, count).await;
            match result {
                Ok((groups, seed_rows, failure_rows)) => {
                    let blobs: Vec<Vec<u8>> = groups
                        .iter()
                        .flat_map(|group| group.blobs.iter().cloned())
                        .collect();
                    coverage.set(coverage_map(&blobs).ok());
                    points.set(sample_points(&blobs, 3_000).unwrap_or_default());
                    processed.set(groups);
                    seeds.set(seed_rows);
                    failures.set(failure_rows);
                    stage.set(Stage::Inspector);
                    message.set(None);
                }
                Err(error) => message.set(Some(error)),
            }
            busy.set(false);
        });
    };

    let upload = move |_| {
        let groups = processed.get();
        if groups.is_empty() || busy.get() {
            return;
        }
        let settings = match group_settings(
            &body.get(),
            &firmware.get(),
            &sim.get(),
            &controls.get(),
            intent_recipe.get(),
            &recipe_target.get(),
        ) {
            Ok(settings) => settings,
            Err(error) => {
                message.set(Some(error));
                return;
            }
        };
        let images = prepared_images(&groups);
        let files = all_files.get();
        let seed_rows = seeds.get();
        let opt_in = serial_opt_in.get();
        busy.set(true);
        message.set(Some("Waiting for the passkey.".to_owned()));
        spawn_local(async move {
            match upload_batch(settings, images, files, seed_rows, opt_in).await {
                Ok(reply) => {
                    let link = reply
                        .status_token
                        .as_ref()
                        .map(|token| format!("/status/{token}"));
                    status_link.set(link);
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
            "Consent recorded locally. Send the downloaded file with the fixture manifest to donate it."
                .to_owned(),
        ));
    };

    view! {
        <h1>"Contribute"</h1>
        <p class="lede">
            "Processing happens on this device. Pixel data never leaves it; the payload inspector shows exactly what will."
        </p>
        <ol class="ladder">
            <li>"Rung 0: point at your library."</li>
            <li>"Rung 1: a few requested shots for coverage gaps."</li>
            <li>"Rung 2: re-render three RAWs through your other sims."</li>
            <li>"Rung 3: phone pattern session."</li>
            <li>"Rung 4: tone gradient."</li>
            <li>"Rung 5: color chart session."</li>
        </ol>
        {move || message.get().map(|text| view! { <p class="notice" role="status">{text}</p> })}

        <section class="step">
            <h2>"1. Drop files or a folder"</h2>
            <label class="button" for="files">"Choose files"</label>
            <input id="files" type="file" multiple accept=".raf,.dng,.jpg,.jpeg,.arw,.cr2,.nef,.orf,.rw2"
                on:change=on_files/>
            <label class="button" for="folder">"Choose a folder"</label>
            <input id="folder" type="file" multiple prop:webkitdirectory=true on:change=on_files/>
            {move || plan.get().filter(|_| stage.get() != Stage::Drop).map(|pairing| view! { <Pairing plan=pairing/> })}
        </section>

        <section class="step" hidden=move || stage.get() == Stage::Drop>
            <h2>"2. Confirm groups"</h2>
            <p>"Native contributions describe the camera's own rendering. Recipe contributions attempt a target look."</p>
            <label for="body-id">"Body id"</label>
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
            <label for="recipe-target">"Target look slug (recipe only)"</label>
            <input id="recipe-target" type="text" prop:value=move || recipe_target.get()
                on:input=move |event| recipe_target.set(event_target_value(&event))/>
            <label for="controls">"Control steps (key=step, one per line)"</label>
            <textarea id="controls" rows="3" prop:value=move || controls.get()
                on:input=move |event| controls.set(event_target_value(&event))></textarea>
            <label for="workers">"Workers"</label>
            <select id="workers" on:change=move |event| {
                worker_count.set(event_target_value(&event).parse().unwrap_or(1))
            }>
                {(1..=max_worker_count()).map(|count| view! {
                    <option value=count.to_string() selected=count == worker_count.get_untracked()>{count}</option>
                }).collect_view()}
            </select>
            <button on:click=process disabled=move || busy.get()>"Process locally"</button>
        </section>

        <section class="step" hidden=move || stage.get() != Stage::Inspector>
            <h2>"3. Payload inspector"</h2>
            <p>
                "Samples cannot be turned back into a photo, but someone who has the original could match them. No account or IP address is stored."
            </p>
            {move || coverage.get().map(|map| view! { <CoverageMapView map/> })}
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
                        view! {
                            <tr class="group-row"><th scope="row" colspan="2">{format!("{} image {}", group.raw, index + 1)}</th></tr>
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
                <thead><tr><th scope="col">RAW</th><th scope="col">Time</th><th scope="col">Worker memory</th></tr></thead>
                <tbody>
                    {move || processed.get().into_iter().map(|group| view! {
                        <tr>
                            <th scope="row">{group.raw}</th>
                            <td>{format!("{:.0} ms", group.millis)}</td>
                            <td>{format!("{:.0} MiB", group.memory as f64 / (1024.0 * 1024.0))}</td>
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
            <h3>"4. Sign and upload"</h3>
            <p>"One passkey gesture signs this batch. Proof of work runs locally before the upload."</p>
            <button on:click=upload disabled=move || busy.get()>"Sign and upload"</button>
            {move || status_link.get().map(|link| view! {
                <p class="notice">
                    "Status link: " <a href=link.clone()>{link.clone()}</a>
                    ". Next rung: a few requested shots for coverage gaps."
                </p>
            })}
        </section>
    }
}

#[component]
fn Pairing(plan: PairingPlan) -> impl IntoView {
    let PairingPlan {
        groups, unpaired, ..
    } = plan;
    let pair_count = groups.len();
    let unpaired_count = unpaired.len();
    view! {
        <div class="pairing">
            <h3>{format!("{pair_count} pair(s) found")}</h3>
            <ul>
                {groups.into_iter().map(|group| view! {
                    <li>
                        <strong>{group.raw}</strong>
                        {format!(" with {} JPEG(s): ", group.jpegs.len())}
                        <span class="muted">{group.jpegs.join(", ")}</span>
                    </li>
                }).collect_view()}
            </ul>
            {(!unpaired.is_empty()).then(|| view! {
                <details>
                    <summary>{format!("{unpaired_count} unpaired file(s)")}</summary>
                    <ul>
                        {unpaired.into_iter().map(|entry| view! {
                            <li>{entry.name} " — " {format!("{:?}", entry.reason)}</li>
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
            <figcaption>{format!("Coverage: {} samples over {} image(s)", map.samples, map.images)}</figcaption>
        </figure>
    }
}

#[component]
fn ColourCloud(points: RwSignal<Vec<[f32; 3]>>) -> impl IntoView {
    let canvas = NodeRef::<Canvas>::new();
    Effect::new(move |_| {
        draw_points(&canvas, &points.get(), Projection::Cloud);
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
        draw_points(&canvas, &points.get(), Projection::Image);
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
    let width = 320u32;
    let height = 240u32;
    canvas.set_width(width);
    canvas.set_height(height);
    let Some(context) = context(&canvas) else {
        return;
    };
    context.set_fill_style_str("#101014");
    context.fill_rect(0.0, 0.0, f64::from(width), f64::from(height));
    if projection == Projection::Image {
        let side = ((points.len() as f64).sqrt().ceil() as u32).max(1);
        let pixel = (width / side).max(1);
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
        let sum = (rgb[0] + rgb[1] + rgb[2]).max(1.0e-6);
        let x = f64::from(rgb[0] / sum);
        let y = f64::from(rgb[1] / sum);
        let px = x * f64::from(width);
        let py = (1.0 - y) * f64::from(height);
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

fn group_settings(
    body: &str,
    firmware: &str,
    sim: &str,
    controls: &str,
    recipe: bool,
    target: &str,
) -> Result<GroupSettings, String> {
    let mut entries = Vec::new();
    for line in controls
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
    Ok(GroupSettings {
        kind: ContributionKind::Library,
        intent: if recipe {
            IntentChoice::Recipe(target.trim().to_owned())
        } else {
            IntentChoice::Native
        },
        body: body.trim().to_owned(),
        firmware: firmware.trim().to_owned(),
        sim: sim.trim().to_owned(),
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
    for worker in &workers {
        worker.terminate();
    }
    Ok((groups, seed_rows, failures))
}

async fn upload_batch(
    settings: GroupSettings,
    images: Vec<PreparedImageView>,
    files: Vec<LocalFile>,
    seeds: Vec<SeedRow>,
    serial_opt_in: bool,
) -> Result<webauthn::UploadReply, String> {
    if images.is_empty() {
        return Err("nothing to upload".to_owned());
    }
    let nonce = webauthn::fetch_nonce().await?;
    let credential_id = match idb::get(idb::StorageKey::CREDENTIAL).await? {
        Some(text) => webauthn::decode_base64url(&text)
            .ok_or_else(|| "the stored credential id is malformed".to_owned())?,
        None => {
            let id = webauthn::create_passkey().await?;
            idb::put(
                idb::StorageKey::CREDENTIAL,
                &webauthn::encode_base64url(&id),
            )
            .await?;
            id
        }
    };
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
    let assertion = webauthn::sign_batch(&challenge, Some(&credential_id)).await?;
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
