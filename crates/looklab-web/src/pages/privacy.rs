// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;

#[component]
pub fn PrivacyPage() -> impl IntoView {
    view! {
        <article class="prose">
            <h1>"Your photos stay with you"</h1>
            <p class="lede">"Browsing, comparing and matching looks is anonymous. Your photos are processed on your device, not uploaded."</p>
            <h2>"When you contribute"</h2>
            <p>"looklab extracts shuffled color samples and a short list of camera settings. No pixels, location, names or capture dates are sent. You can inspect what's being shared before you send it."</p>
            <p>"A profile uses a passkey, with an optional public display name. There's no email address or password. Your browser stores a private sign-in link locally. Keep that link private: anyone with it can read your contribution status."</p>
            <h2>"What others can see"</h2>
            <p>"Individual color samples stay private. Published looks, camera models and aggregate measurements are shared under the data license. Donating an original file as a camera-data test fixture is a separate choice, made for each file."</p>
            <p>
                <a href=crate::base::site_url("/me")>"Your contributions"</a>
                " includes withdrawal. Withdrawing stops your samples counting toward new models; already published versions remain available."
            </p>
            <h2>"Automatic error reports"</h2>
            <p>"If the app fails, server-backed versions send the error category, page category, app build and application code locations to looklab. Reports contain no error messages, console arguments, photos, filenames, private links or profile identifiers. They are sent without sign-in credentials or cookies. Serverless previews send none."</p>
            <p>"Reports stay private. Recent reports are available to the operator for 14 days, with at most 10,000 stored. Older reports are deleted when the next report arrives; backups have their own retention."</p>
            <crate::ui::Disclosure summary="Technical details and independent verification".to_owned()>
            <h2 id="limits">"What a served client can and cannot prove"</h2>
            <p>
                "Subresource Integrity and published build hashes protect against a tampered CDN. They cannot protect against a malicious origin, because the hashes come from that same origin. Client build hashes are not published yet; when the publisher emits them they will appear in the "
                <a href=crate::base::site_url("/community/log")>"audit log"</a>"."
            </p>
            <h2 id="cli">"The CLI alternative"</h2>
            <p>
                "Privacy-sensitive users should build the CLI from source. Sampling is random, so payloads differ between runs. To check that the web client sent only what it should:"
            </p>
            <ol>
                <li>"Open devtools, turn on Preserve log, reload the site, then export the request body actually sent (a HAR export). Do not use the inspector's Download payload: a malicious client could offer a clean download while uploading something else."</li>
                <li>"Export the local seed file. It holds a seed per image or re-render group, plus the prior-transform version."</li>
                <li>"Run looklab-cli verify-payload --seeds seeds.json --module looklab_prep_module.wasm --origin https://looklab.example --rp-id looklab.example --har upload.har <files>. It runs the source-built prep module under wasmtime, parses the canonical envelope strictly, and checks the manifest and every sample blob byte for byte."</li>
            </ol>
            <p>
                "verify-payload checks the WebAuthn fields structurally and never trusts a key served by the site. Unknown client_data_json keys or authenticator extensions are printed for review rather than passed or failed. It lists every other request in the HAR to the site's origin or elsewhere."
            </p>
            <h2 id="published">"What is published"</h2>
            <p>
                "Per-image samples are never published. Fitted targets, body parameters, control-response models and aggregate statistics are, under the data license. Fixture donation is a separate, explicit, per-file opt-in."
            </p>
            <h2 id="source">"Source"</h2>
            <p>
                <a href="https://github.com/skeet70/looklab">"looklab on GitHub"</a>" - AGPL-3.0-or-later. Published looks and previews are static files. Sharing and profiles use the looklab server."
            </p>
            </crate::ui::Disclosure>
        </article>
    }
}
