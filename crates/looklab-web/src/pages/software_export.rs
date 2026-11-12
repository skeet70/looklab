// SPDX-License-Identifier: AGPL-3.0-or-later

use std::io::{Cursor, Write};
use std::sync::Arc;

use leptos::prelude::*;
use leptos_router::hooks::use_query_map;
use looklab_export::CctK;
use looklab_model::file::{LookVersion, Slug};
use looklab_publish::{
    ArtifactHash, InputTransformEntry, InterfaceProfileEntry, InterfaceProfileManifest,
    RoundTripReport, SelectionScope, SiteIndex,
};
use looklab_schema::{BodyId, Format, IlluminantHandling, InsertionPoint};
use looklab_wasm::{
    ExportFileName, ExportFormat, ExportIlluminant, SoftwareExportReply, SoftwareExportRequest,
    SoftwareExportVersion, decode_software_export_reply, encode_software_export_request,
};
use serde::{Deserialize, Serialize};
use zip::write::SimpleFileOptions;

use crate::{data, ui};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SoftwareReplay {
    version: SoftwareExportVersion,
    target: Slug,
    target_version: LookVersion,
    target_manifest: ArtifactHash,
    profile: InterfaceProfileEntry,
    body: Option<BodyId>,
    input_transform: Option<InputTransformEntry>,
    format: ExportFormat,
    cct: CctK,
}

struct CompletedExport {
    pin: SoftwareReplay,
    reply: SoftwareExportReply,
    scope: SelectionScope,
    blockers: Vec<String>,
    definition: String,
    manifest: InterfaceProfileManifest,
    reference: RoundTripReport,
    reference_report: Vec<u8>,
    insertion: InsertionPoint,
    illuminant_handling: IlluminantHandling,
}

fn requested_replay(value: Option<String>) -> Result<Option<SoftwareReplay>, String> {
    value
        .map(|encoded| {
            let bytes = crate::webauthn::decode_base64url(&encoded)
                .ok_or("The pinned export link is not valid base64url.")?;
            serde_json::from_slice(&bytes)
                .map_err(|error| format!("The pinned export link is invalid: {error}"))
        })
        .transpose()
}

fn format_choice(value: &str) -> Result<ExportFormat, String> {
    match value {
        "cube" => Ok(ExportFormat::Cube),
        "haldclut" => Ok(ExportFormat::Haldclut),
        "native_json" => Ok(ExportFormat::NativeJson),
        _ => Err("Choose a supported export format.".to_owned()),
    }
}

fn format_from_schema(value: Format) -> ExportFormat {
    match value {
        Format::Cube => ExportFormat::Cube,
        Format::Haldclut => ExportFormat::Haldclut,
        Format::NativeJson => ExportFormat::NativeJson,
    }
}

fn camera_label(index: &SiteIndex, body: &BodyId) -> String {
    index
        .bodies
        .iter()
        .find(|entry| entry.id == *body)
        .map(|entry| format!("{} {}", entry.make, entry.model))
        .unwrap_or_else(|| body.as_str().replace(['/', '-'], " "))
}

fn format_label(format: ExportFormat) -> &'static str {
    match format {
        ExportFormat::Cube => "Cube LUT",
        ExportFormat::Haldclut => "HaldCLUT",
        ExportFormat::NativeJson => "native JSON",
    }
}

fn insertion_label(point: InsertionPoint) -> &'static str {
    match point {
        InsertionPoint::CameraRgbWbLinear => "camera RGB after white balance",
        InsertionPoint::SceneLinearRec2020 => "scene-linear Rec. 2020",
        InsertionPoint::SceneLinearXyzD65 => "scene-linear XYZ (D65)",
        InsertionPoint::DisplayEncodedPostTone => "display-encoded after tone mapping",
    }
}

fn illuminant_label(handling: IlluminantHandling, bake: &looklab_wasm::ExportBake) -> String {
    match handling {
        IlluminantHandling::Single => {
            format!(
                "single illuminant at {} K",
                bake.primary_cct.kelvin() as u32
            )
        }
        IlluminantHandling::DualInterpolated => "dual illuminant (StdA and D65)".to_owned(),
    }
}

#[component]
pub(super) fn SoftwareExportPanel(
    index: SiteIndex,
    slug: Slug,
    version: LookVersion,
) -> impl IntoView {
    let query = use_query_map();
    let replay = match requested_replay(query.get_untracked().get("export")) {
        Ok(Some(pin)) if pin.target != slug || pin.target_version != version => {
            return ui::failure(
                "The export link names a different target. No other target was substituted.",
            )
            .into_any();
        }
        Ok(pin) => pin,
        Err(error) => return ui::failure(&error).into_any(),
    };
    let mut profiles = index.interfaces.clone();
    if let Some(pin) = &replay
        && profiles
            .iter()
            .all(|entry| entry.manifest_sha256 != pin.profile.manifest_sha256)
    {
        profiles.push(pin.profile.clone());
    }
    let Some(initial) = replay
        .as_ref()
        .map(|pin| pin.profile.clone())
        .or_else(|| profiles.first().cloned())
    else {
        return view! {
            <ui::EmptyState title="No editing-app profiles are published yet".to_owned() message="A published pipeline definition and reference check are needed before building an editing-app export.".to_owned()>
                <a href=crate::base::site_url("/developers/pipelines")>"About editing-app profiles"</a>
            </ui::EmptyState>
        }.into_any();
    };
    let catalog = StoredValue::new(index);
    let profiles = StoredValue::new(profiles);
    let target = StoredValue::new((slug, version));
    let replay = RwSignal::new(replay);
    let selected = RwSignal::new(initial);
    let loaded = RwSignal::new(data::Load::<Arc<data::LoadedInterface>>::Loading);
    let body = RwSignal::new(None::<BodyId>);
    let format = RwSignal::new(ExportFormat::Cube);
    let cct = RwSignal::new(CctK::D65);
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let completed = RwSignal::new(None::<Arc<CompletedExport>>);
    Effect::new(move |_| {
        let entry = selected.get();
        let pin = replay.get_untracked();
        loaded.set(data::Load::Loading);
        completed.set(None);
        error.set(None);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            match data::interface_profile(&entry).await {
                Ok(profile) => {
                    body.set(pin.as_ref().and_then(|pin| pin.body.clone()));
                    cct.set(pin.as_ref().map_or(CctK::D65, |pin| pin.cct));
                    format.set(
                        pin.as_ref()
                            .map(|pin| pin.format)
                            .or_else(|| {
                                profile
                                    .profile
                                    .formats
                                    .first()
                                    .copied()
                                    .map(format_from_schema)
                            })
                            .unwrap_or(ExportFormat::Cube),
                    );
                    loaded.set(data::Load::Ready(Arc::new(profile)));
                }
                Err(message) => loaded.set(data::Load::Failed(message)),
            }
        });
    });
    let build = move |_| {
        if busy.get_untracked() {
            return;
        }
        let data::Load::Ready(profile) = loaded.get_untracked() else {
            return;
        };
        let (slug, version) = target.get_value();
        let pin = replay.get_untracked();
        let camera = body.get_untracked();
        let chosen_format = format.get_untracked();
        let chosen_cct = cct.get_untracked();
        let transforms = catalog.with_value(|index| index.input_transforms.clone());
        busy.set(true);
        error.set(None);
        completed.set(None);
        leptos::task::spawn_local_scoped_with_cancellation(async move {
            let result = async {
                let target = data::export_target(&slug, &version, pin.as_ref().map(|pin| &pin.target_manifest)).await?;
                let needs_camera = profile.profile.insertion.point == InsertionPoint::CameraRgbWbLinear;
                let transform_pin = if needs_camera {
                    let camera = camera.as_ref().ok_or("Choose a camera for this pipeline's camera-RGB input.")?;
                    let transform = match pin.as_ref() {
                        Some(pin) => pin.input_transform.as_ref().cloned().ok_or(
                            "The saved export has no camera input calibration for this pipeline.".to_owned(),
                        )?,
                        None => transforms.into_iter().find(|entry| &entry.body == camera)
                            .ok_or("No camera input calibration is published for this camera. This pipeline needs one; it does not need a camera settings model.")?,
                    };
                    if &transform.body != camera {
                        return Err("The camera input calibration names a different camera.".to_owned());
                    }
                    Some(transform)
                } else {
                    if pin.as_ref().is_some_and(|pin| pin.input_transform.is_some()) {
                        return Err("This pinned export supplies an input calibration its pipeline does not consume.".to_owned());
                    }
                    None
                };
                let transform_yaml = match &transform_pin {
                    Some(entry) => {
                        let expected = looklab_publish::input_transform_path(&entry.sha256).map_err(|error| error.to_string())?;
                        if entry.path != expected {
                            return Err("The input calibration path differs from its content address.".to_owned());
                        }
                        let bytes = data::published_raw(&entry.path).await?;
                        entry.verify(&bytes).map_err(|error| error.to_string())?;
                        Some(String::from_utf8(bytes).map_err(|error| error.to_string())?)
                    }
                    None => None,
                };
                let request = SoftwareExportRequest {
                    version: SoftwareExportVersion::CURRENT,
                    target_look: target.bytes,
                    profile_yaml: profile.definition.clone(),
                    transform_yaml,
                    body: camera.clone(),
                    format: chosen_format,
                    cct: chosen_cct,
                };
                if pin.as_ref().is_some_and(|pin| pin.version != request.version) {
                    return Err("This export engine cannot replay the pinned engine version.".to_owned());
                }
                let worker = crate::worker::MatchWorker::spawn()?;
                let bytes = worker.run_export(&encode_software_export_request(&request)).await.map_err(|error| error.0)?;
                let reply = decode_software_export_reply(&bytes)?;
                if reply.version != request.version || reply.target_slug != slug || reply.target_version != version
                    || reply.profile_id != profile.manifest.id || reply.profile_version != profile.manifest.version
                    || reply.format != chosen_format
                {
                    return Err("The export reply differs from its pinned inputs.".to_owned());
                }
                if reply.input_transform_version != transform_pin.as_ref().map(|entry| entry.version) {
                    return Err("The camera input calibration version differs from the saved one.".to_owned());
                }
                let expected_illuminant = match profile.profile.insertion.illuminant_handling {
                    IlluminantHandling::Single => ExportIlluminant::Single,
                    IlluminantHandling::DualInterpolated => ExportIlluminant::DualInterpolated,
                };
                let expected_primary = match profile.profile.insertion.illuminant_handling {
                    IlluminantHandling::Single => chosen_cct,
                    IlluminantHandling::DualInterpolated => CctK::D65,
                };
                let expected_ccts: &[CctK] = match profile.profile.insertion.illuminant_handling {
                    IlluminantHandling::Single => std::slice::from_ref(&chosen_cct),
                    IlluminantHandling::DualInterpolated => &[CctK::STD_A, CctK::D65],
                };
                if reply.bake.illuminant != expected_illuminant
                    || reply.bake.primary_cct != expected_primary
                    || reply.bake.ccts.as_slice() != expected_ccts
                {
                    return Err("The export reply's bake differs from the requested illuminant.".to_owned());
                }
                let mut blockers = target.blockers;
                let scope = if transform_pin.is_some() {
                    blockers.push("This export uses the camera's published input calibration. No check covers that calibration for this export.".to_owned());
                    if target.scope == SelectionScope::Qualified { SelectionScope::Experimental } else { target.scope }
                } else { target.scope };
                Ok(Arc::new(CompletedExport {
                    pin: SoftwareReplay {
                        version: request.version,
                        target: slug,
                        target_version: version,
                        target_manifest: target.manifest_sha256,
                        profile: profile.pin.clone(),
                        body: camera,
                        input_transform: transform_pin,
                        format: chosen_format,
                        cct: chosen_cct,
                    },
                    reply,
                    scope,
                    blockers,
                    definition: profile.definition.clone(),
                    manifest: profile.manifest.clone(),
                    reference: profile.report.clone(),
                    reference_report: profile.report_bytes.clone(),
                    insertion: profile.profile.insertion.point,
                    illuminant_handling: profile.profile.insertion.illuminant_handling,
                }))
            }.await;
            busy.set(false);
            match result {
                Ok(result) => completed.set(Some(result)),
                Err(message) => error.set(Some(message)),
            }
        });
    };
    view! {
        <section aria-labelledby="software-export-heading">
            <h3 id="software-export-heading">"Editing-app export"</h3>
            <p>"Choose the pipeline your app implements. The export is checked locally before download; your photos are not uploaded."</p>
            <fieldset disabled=move || busy.get()>
                <label for="export-profile">"Editing-app pipeline"</label>
                <select id="export-profile" on:change=move |event| {
                    let choice = match ArtifactHash::parse(&event_target_value(&event)) {
                        Ok(choice) => choice,
                        Err(_) => { error.set(Some("Choose a published editing-app profile.".to_owned())); return; }
                    };
                    let entry = profiles.with_value(|entries| entries.iter().find(|entry| entry.manifest_sha256 == choice).cloned());
                    if let Some(entry) = entry { replay.set(None); selected.set(entry); }
                }>
                    {profiles.get_value().into_iter().map(|entry| {
                        let identity = entry.manifest_sha256.clone();
                        view! { <option value=entry.manifest_sha256.to_string() prop:selected=move || selected.get().manifest_sha256 == identity>{entry.name}</option> }
                    }).collect_view()}
                </select>
                {move || match loaded.get() {
                    data::Load::Loading => ui::loading().into_any(),
                    data::Load::Failed(message) => ui::failure(&message).into_any(),
                    data::Load::Ready(profile) => {
                        let needs_camera = profile.profile.insertion.point == InsertionPoint::CameraRgbWbLinear;
                        let needs_body = needs_camera || !profile.profile.bodies.is_empty();
                        let cameras = if profile.profile.bodies.is_empty() {
                            catalog.with_value(|index| index.bodies.iter().filter(|body| body.kind == looklab_publish::BodyKind::Camera).map(|body| body.id.clone()).collect::<Vec<_>>())
                        } else { profile.profile.bodies.clone() };
                        let formats = profile.profile.formats.clone();
                        let single = profile.profile.insertion.illuminant_handling == IlluminantHandling::Single;
                        view! {
                            {needs_body.then(|| view! {
                                <label for="export-camera">"Camera for this pipeline"</label>
                                <select id="export-camera" on:change=move |event| {
                                    let value = event_target_value(&event);
                                    body.set(BodyId::new(&value).ok()); replay.set(None); completed.set(None);
                                }>
                                    <option value="" prop:selected=move || body.get().is_none()>"Choose a camera"</option>
                                    {cameras.into_iter().map(|camera| {
                                        let label = catalog.with_value(|index| camera_label(index, &camera));
                                        let identity = camera.clone();
                                        view! { <option value=camera.to_string() prop:selected=move || body.get().as_ref() == Some(&identity)>{label}</option> }
                                    }).collect_view()}
                                </select>
                                <p class="muted">{if needs_camera { "This pipeline uses a published camera input calibration, not a stock-settings model." } else { "This pipeline is declared for these cameras; it uses a fixed scene-space conversion, not their settings models." }}</p>
                            })}
                            <label for="export-format">"File format"</label>
                            <select id="export-format" on:change=move |event| {
                                match format_choice(&event_target_value(&event)) {
                                    Ok(value) => { format.set(value); replay.set(None); completed.set(None); }
                                    Err(message) => error.set(Some(message)),
                                }
                            }>
                                {formats.into_iter().map(format_from_schema).map(|choice| view! { <option value=choice.to_string() prop:selected=move || format.get() == choice>{choice.to_string()}</option> }).collect_view()}
                            </select>
                            {single.then(|| view! {
                                <label for="export-cct">"Lighting for this export"</label>
                                <select id="export-cct" on:change=move |event| {
                                    if let Some(value) = event_target_value(&event).parse::<f64>().ok().and_then(CctK::new) {
                                        cct.set(value); replay.set(None); completed.set(None);
                                    }
                                }>
                                    {[(CctK::D65, "Reference daylight (6500 K)"), (CctK::DAYLIGHT, "Daylight (5500 K)"), (CctK::TUNGSTEN, "Tungsten (3200 K)")].into_iter().map(|(value, label)| view! {
                                        <option value=value.kelvin().to_string() prop:selected=move || cct.get() == value>{label}</option>
                                    }).collect_view()}
                                </select>
                            })}
                            {(!single).then(|| view! { <p>"This pipeline uses both standard illuminants. Both artifacts are included in the download."</p> })}
                            <button type="button" on:click=build>"Build and check export"</button>
                        }.into_any()
                    }
                }}
            </fieldset>
            {move || busy.get().then(|| view! { <p role="status">"Building and checking this pipeline on your device…"</p> })}
            {move || error.get().map(|message| ui::failure(&message))}
            {move || completed.get().map(|result| view! { <SoftwareExportResult result/> })}
        </section>
    }.into_any()
}

#[derive(Serialize)]
struct OutputArtifact<'a> {
    name: &'a ExportFileName,
    sha256: ArtifactHash,
    bytes: u64,
}

#[derive(Serialize)]
struct DownloadMetadata<'a> {
    inputs: &'a SoftwareReplay,
    profile_manifest: &'a InterfaceProfileManifest,
    profile_reference: &'a RoundTripReport,
    scope: SelectionScope,
    blockers: &'a [String],
    bake: &'a looklab_wasm::ExportBake,
    validation: &'a looklab_wasm::ExportValidation,
    artifacts: Vec<OutputArtifact<'a>>,
}

fn archive(result: &CompletedExport) -> Result<Vec<u8>, String> {
    if result.reply.flagged || result.reply.files.is_empty() {
        return Err(
            "This export did not pass validation and has no downloadable artifacts.".to_owned(),
        );
    }
    let metadata = DownloadMetadata {
        inputs: &result.pin,
        profile_manifest: &result.manifest,
        profile_reference: &result.reference,
        scope: result.scope,
        blockers: &result.blockers,
        bake: &result.reply.bake,
        validation: &result.reply.validation,
        artifacts: result
            .reply
            .files
            .iter()
            .map(|file| OutputArtifact {
                name: &file.name,
                sha256: ArtifactHash::of(&file.bytes),
                bytes: file.bytes.len() as u64,
            })
            .collect(),
    };
    let metadata = serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?;
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    for (name, bytes) in [
        ("export.json", metadata.as_slice()),
        ("profile.yaml", result.definition.as_bytes()),
        ("reference-report.json", result.reference_report.as_slice()),
    ] {
        writer
            .start_file(name, options)
            .map_err(|error| error.to_string())?;
        writer.write_all(bytes).map_err(|error| error.to_string())?;
    }
    for file in &result.reply.files {
        writer
            .start_file(format!("artifacts/{}", file.name.as_str()), options)
            .map_err(|error| error.to_string())?;
        writer
            .write_all(&file.bytes)
            .map_err(|error| error.to_string())?;
    }
    writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| error.to_string())
}

#[component]
fn SoftwareExportResult(result: Arc<CompletedExport>) -> impl IntoView {
    let message = RwSignal::new(None::<String>);
    let download = result.clone();
    let replay = serde_json::to_vec(&result.pin).map(|bytes| {
        crate::base::site_url(&format!(
            "/match?apply=software&look={}&version={}&export={}",
            crate::base::query_component(result.pin.target.as_str()),
            crate::base::query_component(result.pin.target_version.as_str()),
            crate::webauthn::encode_base64url(&bytes)
        ))
    });
    let reports = std::iter::once(result.reply.validation.primary)
        .chain(result.reply.validation.others.iter().copied())
        .collect::<Vec<_>>();
    let unsupported = reports
        .iter()
        .map(|report| report.unsupported)
        .max()
        .unwrap_or(0);
    let reference_status = if result.reference.flagged {
        "exceeds the export error limit"
    } else {
        "passes the export error limit"
    };
    view! {
        <section aria-labelledby="software-result-heading">
            <h3 id="software-result-heading">{if result.reply.flagged { "This export did not pass validation" } else { "Your editing-app export" }}</h3>
            <p>{data::selection_scope_label(result.scope)}</p>
            {result.blockers.iter().map(|message| view! { <p class="notice">{message.clone()}</p> }).collect_view()}
            <p>{format!("Applies to {}. Input: {}. {}.", format_label(result.pin.format), insertion_label(result.insertion), illuminant_label(result.illuminant_handling, &result.reply.bake))}</p>
            <p>"Validation simulates the declared pipeline. It does not confirm an editing app's implementation or a camera's accuracy."</p>
            {(unsupported > 0).then(|| view! { <p class="notice">{format!("{unsupported} test colors are outside this pipeline's representable region or unsupported by the target and were not scored.")}</p> })}
            {result.reply.validation.tone_mismatch.map(|stats| view! { <p class="notice">{format!("This post-tone insertion cannot reproduce the full tone curve. Tone mismatch: median {:.2}, p95 {:.2} ΔE.", stats.median, stats.p95)}</p> })}
            {(!result.reply.flagged).then(|| view! {
                <button type="button" on:click=move |_| {
                    if let Err(error) = archive(&download).and_then(|bytes| crate::files::download("looklab-export.zip", &bytes, "application/zip")) { message.set(Some(error)); }
                }>"Download validated export (.zip)"</button>
            })}
            {replay.ok().map(|href| view! { <p><a href=href>"Open this pinned export"</a></p> })}
            <ui::Disclosure summary="Validation and exact inputs".to_owned()>
                <p>{format!("Target {} {} · manifest {}", result.pin.target, result.pin.target_version, result.pin.target_manifest)}</p>
                <p>{format!("Pipeline {} · declared version {} · revision {} · engine {}", result.reply.profile_id, result.reply.profile_version, result.pin.profile.manifest_sha256, result.pin.version)}</p>
                <p>{format!("Profile reference check {reference_status} ({} probes, {} clipped targets). It checks the declared pipeline, not this target or an editing app.", result.reference.probes, result.reference.target_clipped)}</p>
                {result.pin.input_transform.as_ref().map(|entry| view! { <p>{format!("Input prior {} version {} · {}", entry.body, entry.version, entry.sha256)}</p> })}
                <ul>{reports.into_iter().map(|report| view! {
                    <li>{format!("{} K: {} supported probes, median {:.2}, p95 {:.2}, max {:.2} ΔE; {} unsupported, {} clipped.", report.cct.kelvin(), report.stats.count, report.stats.median, report.stats.p95, report.stats.max, report.unsupported, report.clipped)}</li>
                }).collect_view()}</ul>
            </ui::Disclosure>
            {move || message.get().map(|message| view! { <p role="alert">{message}</p> })}
        </section>
    }
}
