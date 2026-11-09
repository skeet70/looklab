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
                <p class="lede">"A look describes how colors and brightness change in a photograph. looklab measures that character, then helps you bring it to another camera."</p>
                <a class="button" href=site_url("/looks")>"Browse looks"</a>
            </header>
            <section>
                <h2>"Measured from real photographs"</h2>
                <p>"A camera’s RAW file records the light it captured. Its JPEG shows how a built-in look rendered that light. Comparing the pair lets us measure contrast, tint, saturation, hue and color brightness."</p>
                <p>"Shared pairs from different cameras and lighting conditions help separate the look from the camera that made it. More varied photos strengthen the estimate, especially in shadows, highlights and less common colors."</p>
                <p>"Your photos stay on your device. When you contribute, only shuffled color samples and selected camera settings are sent, after you review them."</p>
                <a href=site_url("/contribute")>"Help with photos you already have"</a>
            </section>
            <section>
                <h2>"See the character, then get the settings"</h2>
                <p>"Compare looks on the same sample photograph, or choose your camera to get a recipe. Your camera’s controls may not reach every part of another camera’s look. The result explains what is close and what will still differ."</p>
                <p>"Every preview is simulated by our model. It is not a photograph taken by your camera at those settings. Checking with your own shots measures the result and can improve the suggestion."</p>
                <a href=site_url("/match")>"Get a look on your camera"</a>
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
