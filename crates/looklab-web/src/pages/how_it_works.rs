// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::{base::site_url, ui::Disclosure};
use leptos::prelude::*;

#[component]
pub fn HowItWorksPage() -> impl IntoView {
    view! {
        <article class="developers-page developer-prose">
            <header>
                <p class="eyebrow">"Method"</p>
                <h1>"How looks are measured and matched"</h1>
                <p class="lede">"looklab measures camera looks from shared photos, then gives you settings for your camera or a card file for custom firmware."</p>
                <p class="field-row">
                    <a class="button" href=site_url("/looks")>"Browse looks"</a>
                    <a class="button" href=site_url("/community/propose?kind=lut_look")>"Add a custom look"</a>
                    <a class="button" href=site_url("/contribute")>"Help with photos"</a>
                </p>
            </header>
            <ol class="developer-steps">
                <li>
                    <span class="developer-step-number">"01"</span>
                    <h2>"Bring photos"</h2>
                    <p>"Share RAW and straight-from-camera JPEG pairs you already have."</p>
                </li>
                <li>
                    <span class="developer-step-number">"02"</span>
                    <h2>"We measure the look"</h2>
                    <p>"Comparing each pair separates the look from the camera that recorded it."</p>
                </li>
                <li>
                    <span class="developer-step-number">"03"</span>
                    <h2>"You get settings or a card file"</h2>
                    <p>"Use the closest menu settings on your camera, or load the look with custom firmware."</p>
                </li>
            </ol>
            <section>
                <h2>"Two kinds of looks"</h2>
                <p>"Built-in looks are the film simulations and picture styles a camera ships with, measured from RAW and straight-from-camera (SOOC) JPEG pairs. Photos from many cameras help separate the look from the sensor that recorded it."</p>
                <p>"Custom looks are .cube LUTs that people made and share. They need no photos of their own, except to measure the look they were built on."</p>
                <p>"A LUT made on top of a film simulation depends on that simulation. Until it's measured, your look can be previewed but has no settings or card file. Photos of that simulation help."</p>
            </section>
            <section>
                <h2>"What you get"</h2>
                <dl class="developer-definition-list">
                    <dt>"In-camera settings"</dt>
                    <dd>"The closest menu settings on your camera."</dd>
                    <dt>"Card file"</dt>
                    <dd>"A .llk file custom firmware loads as an in-camera look, reaching more of the camera's pipeline than menu settings."</dd>
                    <dt>"Editing app"</dt>
                    <dd>"An export for apps with a published pipeline."</dd>
                </dl>
                <p>"Menu settings are as close as your camera's own controls can get. They may not reach every part of another camera's look."</p>
                <p class="field-row">
                    <a class="button" href=site_url("/match")>"Get the look"</a>
                    <a href=site_url("/developers")>"For developers"</a>
                </p>
            </section>
            <section>
                <h2>"Why straight-from-camera JPEGs"</h2>
                <p>"A straight-from-camera JPEG shows how the camera rendered its RAW. An edited or exported JPEG shows your edit, so it can't measure the camera's look."</p>
                <p>
                    "Presets from Lightroom, RapidRAW or Capture One can't be added directly. "
                    <a href=site_url("/community/propose?kind=lut_look#from-a-preset")>"Convert the preset to a .cube LUT"</a>
                    ", then add it as a custom look. Global color and tone carry over; masks, vignettes, grain and sharpening don't."
                </p>
            </section>
            <section>
                <h2>"Where things stand"</h2>
                <p>"Each look is measured per camera. It publishes for a camera as independent photo sets for that camera arrive, and more photos widen what the estimate covers. The number of photos behind a look says how much evidence exists, not how accurate the result is."</p>
                <p>"There's nothing to send us. Your browser reads the photos here and sends only color samples, after you review them. Large folders are fine."</p>
                <p class="field-row">
                    <a class="button" href=site_url("/contribute/wanted")>"See which shots we need"</a>
                    <a href=site_url("/contribute")>"Help with photos"</a>
                </p>
            </section>
            <section>
                <h2>"For firmware projects"</h2>
                <p>"A card file is not tied to one camera. Your firmware reads the card and translates it into its own pipeline, so the look can reach more of the camera than menu settings do."</p>
                <p>"Card loaders are written by firmware projects. The firmware already verified from tagged renders is listed on For developers, together with the format to implement."</p>
                <a class="button" href=site_url("/developers")>"For developers"</a>
            </section>
            <section>
                <h2>"What does “close” mean?"</h2>
                <p>"The labels describe measured color differences, from the smallest to the largest:"</p>
                <ol class="closeness-explanation">
                    <li>
                        <strong>"Indistinguishable"</strong>
                        <span>"The smallest measured differences."</span>
                    </li>
                    <li>
                        <strong>"Nearly identical"</strong>
                        <span>"A small difference remains."</span>
                    </li>
                    <li>
                        <strong>"Close"</strong>
                        <span>"Similar overall, with some differences."</span>
                    </li>
                    <li>
                        <strong>"Noticeable difference"</strong>
                        <span>"The difference is easier to see."</span>
                    </li>
                    <li>
                        <strong>"Different look"</strong>
                        <span>"The rendering has a different character."</span>
                    </li>
                </ol>
                <p>"An estimate can span two labels when the evidence is uncertain. “Early estimate” means the data is still limited. Measurements show the numbers and their uncertainty when you want to look deeper."</p>
            </section>
            <section>
                <h2>"Evidence you can follow"</h2>
                <p>"Community members contribute measurements and propose changes. Moderators review changes to camera data. New contributors’ influence is limited until a trusted member vouches for them."</p>
                <p>"Each published version records its evidence and history. Public decisions are recorded in an audit log, including reversions when something needs to be corrected."</p>
                <a href=site_url("/community/log")>"Read the public audit log"</a>
                <Disclosure summary="Technical details and source documents".to_owned()>
                    <p>"Every artifact is checked against its version manifest before use. Hashes detect changed or corrupted files; they do not prove that a measurement is correct. Review, release checks and the public history address the evidence behind each version."</p>
                    <ul>
                        <li>
                            <a href="https://github.com/skeet70/looklab/blob/main/docs/02-color-pipeline.md">
                                "Color pipeline and measurements"
                            </a>
                        </li>
                        <li>
                            <a href="https://github.com/skeet70/looklab/blob/main/docs/04-matching-and-export.md">
                                "Matching and exports"
                            </a>
                        </li>
                        <li>
                            <a href="https://github.com/skeet70/looklab/blob/main/docs/05-trust-and-integrity.md">
                                "Trust and integrity"
                            </a>
                        </li>
                        <li>
                            <a href="https://github.com/skeet70/looklab/blob/main/docs/06-privacy-and-payload.md">
                                "Privacy and shared data"
                            </a>
                        </li>
                    </ul>
                </Disclosure>
            </section>
        </article>
    }
}
