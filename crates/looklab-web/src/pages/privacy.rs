// SPDX-License-Identifier: AGPL-3.0-or-later

use leptos::prelude::*;

#[component]
pub fn PrivacyPage() -> impl IntoView {
    view! {
        <article class="prose">
            <h1>"Privacy and client integrity"</h1>
            <p>
                "Browsing, matching and previews are anonymous and upload nothing. A credential is created only at the first contribution."
            </p>
            <h2 id="limits">"What a served client can and cannot prove"</h2>
            <p>
                "Subresource Integrity and published build hashes protect against a tampered CDN. They cannot protect against a malicious origin, because the hashes come from that same origin. Reproducible-build hashes are published in the "
                <a href="/log">"audit log"</a>"."
            </p>
            <h2 id="cli">"The CLI alternative"</h2>
            <p>
                "Privacy-sensitive users should build the CLI from source. Sampling is random, so payloads differ between runs. To check that the web client sent only what it should:"
            </p>
            <ol>
                <li>"Open devtools, turn on Preserve log, reload the site, then export the request body actually sent (a HAR export). Do not use the inspector's Download payload: a malicious client could offer a clean download while uploading something else."</li>
                <li>"Export the local seed file. It holds a seed per image or re-render group, plus the prior-transform version."</li>
                <li>"Run looklab-cli verify-payload --seeds seeds.json --har upload.har <files>. It runs the source-built prep module under wasmtime, parses the canonical envelope strictly, and checks the manifest and every sample blob byte for byte."</li>
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
                <a href="https://github.com/skeet70/looklab">"looklab on GitHub"</a>" - AGPL-3.0-or-later. This site reads only static JSON, artifacts and scene files; it has no server session."
            </p>
        </article>
    }
}
