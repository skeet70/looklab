// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;
use looklab_export::card::read_card;
use looklab_meta::{RenderTagOutcome, classify_render_tag};
use serde::{Deserialize, Deserializer};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::File;

use crate::{
    base::site_url,
    data,
    ui::{DeltaE, Disclosure, DropZone, EmptyState, Notice, NoticeKind, closeness_level},
};

const SPEC: &str = "https://github.com/skeet70/looklab/blob/main/docs/04-matching-and-export.md#8-firmware-card-format";

#[derive(Clone, Copy)]
enum TagStatus {
    Absent,
    Malformed,
    Valid,
    Checking,
    Error,
}

impl TagStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Malformed => "malformed",
            Self::Valid => "valid",
            Self::Checking => "checking",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct SelectionGeneration(u64);

impl SelectionGeneration {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Clone, Deserialize)]
struct ConformanceArchive {
    archive: looklab_publish::SitePath,
    sha256: looklab_publish::ArtifactHash,
}

#[component]
fn ConformanceDownload() -> impl IntoView {
    let archive = RwSignal::new(data::Load::Loading);
    spawn_local(async move {
        let result = async {
            let response =
                gloo_net::http::Request::get(&site_url("/data/developers/conformance-v1.json"))
                    .send()
                    .await
                    .map_err(|error| error.to_string())?;
            if !response.ok() {
                return Err(format!("HTTP {}", response.status()));
            }
            response
                .json::<ConformanceArchive>()
                .await
                .map_err(|error| error.to_string())
        }
        .await;
        archive.set(match result {
            Ok(download) => data::Load::Ready(download),
            Err(error) => data::Load::Failed(error),
        });
    });
    view! {
        <div class="conformance-download" aria-live="polite">
            {move || match archive.get() {
                data::Load::Loading => view! {
                    <p role="status">"Loading conformance download…"</p>
                }.into_any(),
                data::Load::Failed(error) => view! {
                    <p role="alert">{format!("Conformance download unavailable: {error}")}</p>
                    <a href="https://github.com/skeet70/looklab/tree/main/data/card/conformance-v1">
                        "Browse conformance vectors in the repository"
                    </a>
                }.into_any(),
                data::Load::Ready(download) => view! {
                    <a href=site_url(&format!("/{}", download.archive.as_str())) download="looklab-conformance-v1.zip">
                        <strong>"Download conformance vectors"</strong>
                        <span>"ZIP: valid and malformed cards, grid outputs and render tags"</span>
                    </a>
                    <Disclosure summary="Download SHA-256".to_owned()>
                        <code class="conformance-sha256">{download.sha256.as_str().to_owned()}</code>
                    </Disclosure>
                }.into_any(),
            }}
        </div>
    }
}

async fn file_bytes(file: &File) -> Result<Vec<u8>, String> {
    JsFuture::from(file.array_buffer())
        .await
        .map(|buffer| js_sys::Uint8Array::new(&buffer).to_vec())
        .map_err(|_| "This file could not be read. Choose it again.".to_owned())
}

fn inspect(bytes: &[u8]) -> Result<String, String> {
    let card = read_card(bytes).map_err(|error| format!("{error:?}: {error}"))?;
    let mut text = format!(
        "Magic: LLCF\nFormat version: 1\nHeader length: 64 bytes\nFile length: {} bytes\nSections: {}\nHeader CRC32: {:08x}\nContent id: {}\nLook: {} @ {}\nName: {}\nShort name: {}\nSemantics: {:?}\nNeutral samples: {}\nFit samples: {}\nGrid: {}\n",
        bytes.len(),
        card.sections().len(),
        u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]),
        card.content_id_hex(),
        card.meta().slug,
        card.meta().version,
        card.meta().name,
        card.meta().short_name,
        card.semantics(),
        card.neutral().len(),
        card.fit().len(),
        if card.grid().is_some() {
            "present"
        } else {
            "not included"
        }
    );
    for section in card.sections() {
        use std::fmt::Write;
        let _ = writeln!(
            text,
            "Section {}: offset {}, {} bytes",
            section.kind, section.offset, section.length
        );
    }
    Ok(text)
}

async fn check_tag(outcome: RenderTagOutcome) -> (TagStatus, String) {
    match outcome {
        RenderTagOutcome::Absent => (TagStatus::Absent, "No looklab render tag found.".to_owned()),
        RenderTagOutcome::Malformed => (TagStatus::Malformed, "Malformed render tag. Check the exact field order, spacing and allowed characters in the specification.".to_owned()),
        RenderTagOutcome::Valid(tag) => {
            let fields = format!("Firmware: {} / {}\nLook: {} @ {}\nContent id prefix: {}", tag.firmware_id, tag.firmware_version, tag.look_slug, tag.look_version, tag.cid_prefix_hex());
            let result = match data::card(tag.look_slug.as_str(), tag.look_version.as_str()).await {
                Ok(bytes) => match read_card(&bytes) {
                    Ok(card) if card.cid_prefix() == tag.cid_prefix => "Matches the published card. The card was checked against its version manifest.".to_owned(),
                    Ok(_) => "Content id mismatch. This render uses a different card and cannot count for the named version.".to_owned(),
                    Err(error) => format!("The published card could not be read: {error}"),
                },
                Err(error) => format!("Published card could not be checked: {error}"),
            };
            (TagStatus::Valid, format!("Valid render tag\n{fields}\n\n{result}"))
        }
    }
}

#[component]
pub fn DevelopersPage() -> impl IntoView {
    let card_result = RwSignal::new(None::<Result<String, String>>);
    let card_name = RwSignal::new(String::new());
    let card_generation = RwSignal::new(SelectionGeneration::default());
    let tag_result = RwSignal::new(None::<(TagStatus, String)>);
    let tag_generation = RwSignal::new(SelectionGeneration::default());
    let raw_tag = RwSignal::new(String::new());
    let card_files = Callback::new(move |files: Vec<File>| {
        if let Some(file) = files.into_iter().next() {
            let generation = card_generation.get_untracked().next();
            card_generation.set(generation);
            card_name.set(file.name());
            card_result.set(None);
            spawn_local(async move {
                let bytes = file_bytes(&file).await;
                if card_generation.try_get_untracked() == Some(generation) {
                    card_result.set(Some(bytes.and_then(|bytes| inspect(&bytes))));
                }
            });
        }
    });
    let jpeg_files = Callback::new(move |files: Vec<File>| {
        if let Some(file) = files.into_iter().next() {
            let generation = tag_generation.get_untracked().next();
            tag_generation.set(generation);
            tag_result.set(Some((
                TagStatus::Checking,
                "Reading JPEG locally…".to_owned(),
            )));
            spawn_local(async move {
                let bytes = file_bytes(&file).await;
                if tag_generation.try_get_untracked() != Some(generation) {
                    return;
                }
                let result = match bytes {
                    Ok(bytes) => match looklab_meta::parse_jpeg(&bytes) {
                        Ok(metadata) => check_tag(metadata.render_tag).await,
                        Err(error) => (
                            TagStatus::Error,
                            format!("JPEG metadata could not be read: {error}"),
                        ),
                    },
                    Err(error) => (TagStatus::Error, error),
                };
                if tag_generation.try_get_untracked() == Some(generation) {
                    tag_result.set(Some(result));
                }
            });
        }
    });
    let check_raw_tag = move |_| {
        let generation = tag_generation.get_untracked().next();
        tag_generation.set(generation);
        tag_result.set(Some((TagStatus::Checking, "Checking tag…".to_owned())));
        let text = raw_tag.get_untracked();
        spawn_local(async move {
            let result = check_tag(classify_render_tag(&text)).await;
            if tag_generation.try_get_untracked() == Some(generation) {
                tag_result.set(Some(result));
            }
        });
    };
    view! {
        <article class="developers-page">
            <header class="developer-hero">
                <p class="eyebrow">"For firmware developers"</p>
                <h1>"Make your firmware load looklab looks"</h1>
                <p class="lede">"One card file describes a look. Your firmware decides how to reproduce it in your camera’s pipeline."</p>
                <a href=site_url("/developers/pipelines")>"Building an editing app instead?"</a>
            </header>
            <ol class="developer-steps">
                <li>
                    <span class="developer-step-number">"01"</span>
                    <h2>"Read a card file"</h2>
                    <p>"Validate the header, sections and content id before using a .llk file. The reference reader and test vectors define the expected result."</p>
                </li>
                <li>
                    <span class="developer-step-number">"02"</span>
                    <h2>"Translate at startup"</h2>
                    <p>"Fit the look to your pipeline’s controls or LUT stage. Cache the translation by content id so it only runs when the card changes."</p>
                </li>
                <li>
                    <span class="developer-step-number">"03"</span>
                    <h2>"Tag every JPEG"</h2>
                    <p>"Write the render tag into EXIF UserComment. It identifies the firmware and card so shared renders can verify your implementation."</p>
                </li>
            </ol>
            <section class="developer-resources">
                <h2>"Implementation resources"</h2>
                <div class="developer-resource-grid">
                    <a href=SPEC>
                        <strong>"Card format specification"</strong>
                        <span>"Version 1 semantics, binary layout and validation rules"</span>
                    </a>
                    <ConformanceDownload/>
                    <a href="https://github.com/skeet70/looklab/tree/main/tools/card-translator-c99">
                        <strong>"C99 example translator"</strong>
                        <span>"A matrix and tone-curve fit, with no allocator"</span>
                    </a>
                </div>
                <Disclosure summary="CLI commands".to_owned()>
                    <pre><code>"looklab-cli card inspect card.llk\nlooklab-cli card eval --help"</code></pre>
                    <p>"Run from a source checkout with cargo run -p looklab-cli -- card inspect card.llk."</p>
                </Disclosure>
            </section>
            <div class="developer-tools">
                <section class="developer-tool" id="card-inspector">
                    <h2>"Card inspector"</h2>
                    <p>"Choose a .llk file to validate it with the reference reader. Your file stays on this device."</p>
                    <DropZone
                        on_files=card_files
                        accept=".llk".to_owned()
                        folders=false
                        prompt="Drop a .llk card file. It's checked on this device.".to_owned()
                    />
                    <div aria-live="polite">
                        {move || card_result.get().map(|result| {
                            let valid = result.is_ok();
                            view! {
                                <div class="inspection-result" data-card-outcome=if valid { "valid" } else { "rejected" }>
                                    <h3>{if valid { "Valid card" } else { "Card rejected" }}</h3>
                                    <p>{card_name.get()}</p>
                                    <pre>{result.unwrap_or_else(|error| error)}</pre>
                                </div>
                            }
                        })}
                    </div>
                    <p class="muted">"Validation stops at the first error. Fix it and inspect the file again to check for further errors."</p>
                </section>
                <section class="developer-tool" id="render-tag-checker">
                    <h2>"Render-tag checker"</h2>
                    <p>"Choose a JPEG to read its EXIF UserComment locally. Only published card data is downloaded to check the content id; your photo is never uploaded."</p>
                    <DropZone
                        on_files=jpeg_files
                        accept="image/jpeg,.jpg,.jpeg".to_owned()
                        folders=false
                        prompt="Drop a JPEG from your firmware. It's read on this device.".to_owned()
                    />
                    <Disclosure summary="Or paste a render tag".to_owned()>
                        <label for="render-tag-text">"EXIF UserComment text"</label>
                        <textarea
                            id="render-tag-text"
                            rows="4"
                            prop:value=move || raw_tag.get()
                            on:input=move |event| {
                                tag_generation.update(|generation| *generation = generation.next());
                                tag_result.set(None);
                                raw_tag.set(event_target_value(&event));
                            }
                        ></textarea>
                        <button type="button" on:click=check_raw_tag>"Check render tag"</button>
                    </Disclosure>
                    <div aria-live="polite">
                        {move || tag_result.get().map(|(outcome, text)| view! {
                            <pre class="inspection-result" data-tag-outcome=outcome.as_str()>{text}</pre>
                        })}
                    </div>
                </section>
            </div>
            <FirmwareListings/>
        </article>
    }
}

#[derive(Clone, Deserialize)]
struct FirmwareVerification {
    firmware_id: looklab_meta::FirmwareId,
    firmware_version: looklab_meta::FirmwareVersion,
    look: looklab_meta::RenderLookSlug,
    look_version: looklab_meta::RenderLookVersion,
    images: u64,
    #[serde(deserialize_with = "difference")]
    median_de: DeltaE,
    #[serde(deserialize_with = "difference")]
    p95_de: DeltaE,
}

#[derive(Deserialize)]
struct VerificationList {
    verifications: Vec<FirmwareVerification>,
}

fn difference<'de, D: Deserializer<'de>>(deserializer: D) -> Result<DeltaE, D::Error> {
    DeltaE::new(f64::deserialize(deserializer)?)
        .ok_or_else(|| serde::de::Error::custom("color difference must be finite and nonnegative"))
}

async fn firmware_verifications() -> Result<Vec<FirmwareVerification>, String> {
    if !crate::base::server_available() {
        return Err("Firmware verification is unavailable in this preview build.".to_owned());
    }
    let response = gloo_net::http::Request::get(&crate::base::api_url("/api/cards/verification"))
        .send()
        .await
        .map_err(|error| format!("The verification service could not be reached: {error}"))?;
    if !response.ok() {
        return Err(format!(
            "The verification service returned HTTP {}. Try again later.",
            response.status()
        ));
    }
    let listing: VerificationList = response
        .json()
        .await
        .map_err(|error| format!("The verification service returned unreadable data: {error}"))?;
    if listing
        .verifications
        .iter()
        .any(|row| row.p95_de.get() < row.median_de.get())
    {
        return Err("The verification service returned inconsistent measurements.".to_owned());
    }
    Ok(listing.verifications)
}

#[component]
fn FirmwareListings() -> impl IntoView {
    let listing = RwSignal::new(data::Load::Loading);
    spawn_local(async move {
        listing.set(match firmware_verifications().await {
            Ok(rows) => data::Load::Ready(rows),
            Err(error) => data::Load::Failed(error),
        });
    });
    view! {
        <section id="firmware-listings">
            <h2>"Listed firmware"</h2>
            <p>"Firmware verified from tagged renders, with the look versions and measured closeness."</p>
            <div aria-live="polite">{move || match listing.get() {
                data::Load::Loading => view! { <p role="status">"Loading firmware verification…"</p> }.into_any(),
                data::Load::Failed(error) => view! { <Notice kind=NoticeKind::Error message=format!("Could not load listed firmware. {error}")/> }.into_any(),
                data::Load::Ready(rows) if rows.is_empty() => view! { <EmptyState title="No firmware listed yet".to_owned() message="Ship the render tag and your users’ renders list it here.".to_owned()/> }.into_any(),
                data::Load::Ready(rows) => view! {
                    <div class="firmware-table-wrap" role="region" aria-label="Firmware verification results" tabindex="0">
                        <table class="firmware-table">
                            <caption>"Verified firmware and card renders"</caption>
                            <thead>
                                <tr>
                                    <th scope="col">"Firmware"</th>
                                    <th scope="col">"Version"</th>
                                    <th scope="col">"Verified look"</th>
                                    <th scope="col">"Photos"</th>
                                    <th scope="col">"Measured closeness"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {rows.into_iter().map(|row| {
                                    let look_url = site_url(&format!("/looks/{}?version={}", row.look, row.look_version));
                                    let measurements = format!(
                                        "Median ΔE (CAM16-UCS): {:.2}. p95: {:.2}. The verification service does not provide an uncertainty band.",
                                        row.median_de.get(), row.p95_de.get()
                                    );
                                    view! {
                                        <tr>
                                            <th scope="row">{row.firmware_id.to_string()}</th>
                                            <td>{row.firmware_version.to_string()}</td>
                                            <td><a href=look_url>{format!("{} {}", row.look, row.look_version)}</a></td>
                                            <td>{row.images}</td>
                                            <td>
                                                {closeness_level(row.median_de).label()}
                                                <Disclosure summary="Measurements".to_owned()>
                                                    <p>{measurements}</p>
                                                </Disclosure>
                                            </td>
                                        </tr>
                                    }
                                }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }.into_any(),
            }}</div>
        </section>
    }
}

#[component]
pub fn PipelinesPage() -> impl IntoView {
    view! {
        <article class="developers-page developer-prose">
            <header>
                <p class="eyebrow">"For editing-app authors"</p>
                <h1>"Bring looks into your editing app"</h1>
                <p class="lede">"An interface profile tells looklab where your app applies a look, what it accepts and what happens afterward."</p>
            </header>
            <section>
                <h2>"Describe your pipeline"</h2>
                <dl class="developer-definition-list">
                    <dt>"Where the look goes"</dt>
                    <dd>"The insertion point, input color space, encoding and white-balance handling."</dd>
                    <dt>"What your app accepts"</dt>
                    <dd>"LUT sizes, interpolation, precision, matrices, curves, file formats and size limits."</dd>
                    <dt>"What happens afterward"</dt>
                    <dd>"Output transforms, tone processing and clipping. A round-trip report measures what the export can and cannot reproduce."</dd>
                </dl>
                <p>"Profiles are for software pipelines. Camera firmware implements the card format instead."</p>
            </section>
            <section>
                <h2>"Published profiles"</h2>
                <EmptyState
                    title="No profiles published in this site data".to_owned()
                    message="An approved editing-app profile describes a supported pipeline and includes its round-trip report.".to_owned()
                />
                <p>
                    <a href="https://github.com/skeet70/looklab/tree/main/data/interfaces">
                        "Browse interface profiles in the repository"
                    </a>
                </p>
            </section>
            <section>
                <h2>"Add your editing app"</h2>
                <p>"Propose a profile as a form or YAML. The submission flow generates a round-trip report for review."</p>
                <a class="button" href=site_url("/community/propose?kind=interface_profile")>
                    "Propose an editing-app profile"
                </a>
                <p>
                    <a href="https://github.com/skeet70/looklab/blob/main/docs/04-matching-and-export.md#6-interface-profiles-software-pipelines">
                        "Read the interface profile specification"
                    </a>
                </p>
            </section>
        </article>
    }
}
