# 06 — Privacy and payload

**Principle:** no pixels leave the user's device, and there are no accounts. The server receives shuffled, coordinate-free color samples and whitelisted metadata, signed by a pseudonymous credential. The service never stores photos, which keeps launch moderation down to metadata, proposals and LUT submissions.

**What this does and doesn't mean.** A sample set can't be turned back into a picture. But anyone holding the original photo could match it to its samples, and the per-image metadata tuple (body, lens, settings, month) is close to unique. So uploads are **pseudonymous personal data** (e.g. under GDPR), not anonymous data. The design minimizes linkability (§1, §6) and doesn't claim more than that.

## 1. What is uploaded

| Data | Form | Why |
|---|---|---|
| Color samples | ≤ 50k (camera RGB, JPEG RGB) pairs + density weight + flags, **shuffled, no coordinates** | The fit |
| Chart/pattern patches | Patch id + mean + std | Anchoring and coverage |
| Body / firmware / lens | Make, model, firmware string, lens model, focal length | Pooling, epochs, consistency |
| Look settings | Whitelisted parsed makernote fields | Realization and control models |
| Exposure | ISO, shutter, aperture, EC, as-shot WB multipliers | Nuisance priors, CCT |
| Capture date | **Month only** | Epoch sanity, drift analysis |
| Software tag | String | Edited-JPEG detection |
| Card render tag | Parsed fields only: format version, firmware id and version, look slug and version, 8-byte content-id prefix ([04 §8.4](04-matching-and-export.md)), plus a `render_tag_malformed` boolean for a recognizable card token that fails parsing; the raw `UserComment` string is dropped | Routing card renders out of every fit; per-firmware verification; keeping a malformed card token from becoming a stock render |
| Re-render group id | Random 16 bytes, client-generated per group | Server-verifiable grouping without a RAW hash |
| Body serial (optional, opt-out) | Sent in a dedicated request field; the server HMACs it on arrival and discards it ([05 §2](05-trust-and-integrity.md)) | Effective-contributor merging for caps only |
| Signature, nonce, proof-of-work | Per batch | Integrity, anti-flood |

**Not uploaded:**
- RAW or content hashes.
- File names or paths.

## 2. What is never uploaded

- Pixels: no thumbnails, previews or crops. RAW files embed full-size preview JPEGs; the client may use them **locally** (e.g. for coverage preselection, [07 §1](07-contribution.md)), and they're never sent.
- Pixel coordinates or spatial order.
- GPS/location, owner/artist/copyright, image unique IDs, face-detection data, raw makernote blobs.
- IP addresses are never stored. Rate limiting happens in memory only.

## 3. Credentials

- Passkeys (WebAuthn) only, created at the first contribution or when the contributor creates a profile in the sign-in dialog ([05 §2](05-trust-and-integrity.md), [08 §4.1](08-ui.md)). No email, no password, no account. Browsing and matching need no credential.
- **Contributor public keys and serial HMACs are never published.** Logs and changelogs use internal ids or display handles.
- **Serials:** the raw value exists only in a type that can be neither serialized nor printed with `Debug`; it's never stored or logged (a test scans the DB and logs). The HMAC is a stable identifier, which is part of why uploads count as pseudonymous personal data.

## 4. Status, withdrawal and erasure

- **Status feed:** at first contribution, the client receives a random **capability token**. `/status/<token>` (and an RSS variant) shows accepted / down-weighted / quarantined per image. The server stores only the token's hash. The token isn't the public key. **Sign-in:** a passkey assertion from the same credential issues an additional token for that browser and leaves the credential's other tokens valid. Creating a profile before its first upload requires proof-of-work on that signed session request, at the same difficulty as a one-image upload by a new-tier credential; signing in with an existing credential does not. New profiles have tier `new` and no roles. **Sign out everywhere:** a signed request revokes every token of the credential. Each response to the credential's own sign-in names its roles, so the client can offer role-gated pages; roles are never published with the credential.
- **Session restoration:** `GET /api/session` with the status token as an `Authorization: Bearer` capability returns that profile's current roles, tier and display handle, with the same token. It creates no token and requires no new assertion. Missing, unknown or revoked tokens disclose no profile metadata.
- **Withdrawal:** a request signed by the contributing credential. The data is excluded from the next fit; blobs are hard-deleted after 30 days; published versions stay immutable.
- **Lost credential:** pending legal review ([10 Q3](10-open-questions.md)). One option is a server-keyed hash of each RAW that lets a user prove possession and delete. But that hash is itself a photo identifier, so it isn't implemented without a decision.

## 5. Client integrity and its limits

- The server validates blob structure, counts and ranges, and rejects any field outside the schema ([03 §2.2](03-data-model.md)).
- Subresource Integrity and published build hashes protect against a tampered CDN. They **can't** protect against a malicious origin, because the hashes come from the same origin. Say so on `/about/privacy`, publish reproducible-build hashes in the audit log, and **point privacy-sensitive users to the CLI**, which they can build from source. Sampling is random, so payloads differ between runs. To check that the web client sent only what it should:
  1. **Open devtools and turn on "Preserve log" before loading the site**, so requests sent on page load are recorded. Then export the **request body actually sent** (HAR export). Don't use the inspector's "Download payload", because a malicious client could offer a clean download while uploading something else.
  2. Export the local **seed file** from the inspector. It contains a seed per image or re-render group, plus the prior-transform version.
  3. Run `looklab-cli verify-payload --seeds seeds.json --har upload.har --module looklab_prep_module.wasm --origin https://site.example --rp-id site.example --data data --kind library --intent native <files>`. Export the HAR with the network panel filtered to the upload session's API requests; any other request in it is flagged. It runs the source-built prep module under wasmtime ([03 §2.1](03-data-model.md)), then:
     - **parses the upload body strictly** as the canonical envelope ([03 §2.3](03-data-model.md)) and rejects any deviation or trailing bytes;
     - checks that the **manifest and every sample blob reproduce byte for byte**. There's no tolerance, since a tolerance could hide data in low-order bits;
     - requires the **batch fields the files determine** to equal the manifest's: body from EXIF make/model against the `--data` snapshot, firmware from EXIF, sim and settings from the makernotes through the body's control schema. `--kind` and `--intent` supply what only you declare; every batch field is printed with its source, and anything neither reproduced nor declared fails;
     - for **intent `card`**, reproduces the card declaration from the files' EXIF `UserComment` render tags; with no tag, `--firmware <id>/<version>` supplies what you declared, and it must equal the manifest's declaration;
     - checks that the remaining fields are the expected set:
       - **nonce:** 32 bytes;
       - **challenge:** the one inside `client_data_json`, which must equal H(nonce ‖ manifest hash);
       - **WebAuthn fields**, checked **structurally**. No check relies on a key served by the site, because a malicious site could generate its own keypair in JavaScript and sign anything:
         - **`client_data_json`** (≤ 1024 bytes), parsed as JSON:
           - required: `type` = `webauthn.get`; `challenge` = the recomputed H(nonce ‖ manifest hash); `origin` = the site's origin; `crossOrigin` false (absent counts as false);
           - accepted extras: `topOrigin`, and Chrome's `other_keys_can_be_added_here` with its fixed text. `topOrigin` must equal an expected top origin when one is declared, else it is printed for review;
           - the bytes must be the WebAuthn serialization (fixed key order, no whitespace, JSON string escapes). Any deviation is printed in full and marked **"review needed"**;
           - **any other key:** printed in full and the result marked **"review needed"** rather than pass/fail. Browsers may legitimately add keys, and data in a JSON key can't be hidden from a person reading it.
         - **`authenticator_data`**, fixed layout per the WebAuthn spec: `rpIdHash` = sha256 of the site's RP ID, then the flags byte and a 4-byte sign count. The attested-credential-data flag must be clear (an assertion carries none). If the extension flag is set, the extensions are decoded and printed for review.
         - **`credential_id`:** identical across all of your uploads. verify-payload records it locally on first use (in its own state file, not anything the site provides) and compares on later runs, or across several HARs given at once.
         - **signature:** must be well-formed for a supported algorithm (Ed25519 64 bytes; ECDSA as valid DER; RSA at the key size). There's no fixed length rule. An optional check against the credential key, if you have it from your own records, is a sanity check for an honest site only; the guarantee doesn't rest on it.
       - **proof-of-work:** k solutions, each verified;
       - **serial:** equal to your files' EXIF serial, or absent;
     - lists **every request** in the HAR, flags any that aren't part of the expected API (nonce, upload, status, passkey registration), and audits what was recorded: a body on an endpoint that takes none, a passkey registration body with keys outside its schema (printed), or a request header outside the browser's own set (printed) is reported.

     **Residual channels (documented, not closed):**
     - a client can choose *which* valid proof-of-work solutions it sends, leaking a few bits per batch at exponentially more work;
     - **the signature itself**: a site that forges keys can choose signature bytes freely (tens to hundreds of bytes per batch, depending on algorithm);
     - **the credential ID chosen at passkey registration**: a forged-key site picks it once (≤ 255 bytes, a one-time leak; it's then fixed by the identical-ID check);
     - timing.
  4. **When this guarantee holds:** only if the prep module uses no WASM relaxed-SIMD (whose results are implementation-defined across engines) and no NaN reaches stored values. Both are enforced in CI ([09 P1.3](09-roadmap.md)).

## 6. Publication

- **Per-image samples aren't published.** Only fitted targets, body parameters, control-response models and aggregate statistics are, under the data license ([10 Q2](10-open-questions.md)). This avoids linking pseudonymous credentials to photos posted elsewhere, and keeps withdrawal meaningful.
- Fixture donation (for makernote mappings, [08 §4](08-ui.md)) is a separate, explicit, per-file opt-in. It **does** share the file, and the consent is recorded in the fixture manifest.
- The upload is signed over the file's name, size and sha256, with its own proof-of-work and a 64 MiB per-file cap. The server stores the bytes privately, keyed by sha256, and the proposal cites them as `committed = false` with a `source` naming that blob. A moderator approval exports the file into the community snapshot under `fixtures/` and marks its manifest entry `committed = true`.

## 7. Payload inspector (required before every upload)

1. **Color cloud:** a 3D scatter of exactly what leaves.
2. **"Can you see the photo?":** samples laid out as an image in upload order, which looks like noise.
3. **Size:** e.g. "41.2 MB → 380 KB", per image and in total.
4. **Metadata table:** every field sent, plus removed fields with their values redacted. The serial row says plainly: "sent once, converted to a keyed hash on arrival, never stored", with an opt-out toggle.
5. **Download payload** (a convenience view with a JSON rendering, not a trust anchor; verify against the HAR, §5) and **export seed file** (local only, never uploaded, labeled sensitive; [03 §2.1](03-data-model.md)).
6. **Plain statement:** samples can't be turned back into a photo, but could be matched by someone who has it. No account or IP is stored.
7. **Source link:** the client code (AGPL) and the CLI alternative.

## 8. Later: public galleries

Post-launch: a separate, explicit, per-image opt-in with downscaled uploads and its own moderation. Launch previews use canonical images owned by the project ([08 §5](08-ui.md)).
