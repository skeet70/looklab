# 06 — Privacy and payload

**Principle:** no pixels leave the user's device, and there are no accounts. The server receives shuffled, coordinate-free color samples and whitelisted metadata, signed by a pseudonymous credential. The service never stores photos, which keeps launch moderation down to metadata, proposals and LUT submissions.

**What this does and doesn't mean.** A sample set can't be turned back into a picture. But anyone holding the original photo could match it to its samples, and the per-image metadata tuple (body, lens, settings, month) is close to unique. So uploads are **pseudonymous personal data** (e.g. under GDPR), not anonymous data. The design minimizes linkability (§1, §6) and doesn't claim more than that.

## 1. What is uploaded

| Data | Form | Why |
|---|---|---|
| Colour or monochrome samples | ≤ 50k (camera RGB, JPEG RGB) pairs + declared output colour space + density weight + flags, **shuffled, no coordinates**; all three output channels retained | Colour fitting, or explicitly authorized monochrome model development |
| Chart/pattern patches | Patch id + mean + std | Anchoring and coverage |
| Body / firmware / lens | Make, model, firmware string, lens model, focal length | Pooling, epochs, consistency |
| Look settings | Whitelisted parsed makernote fields, each marked `reproduced` (decoded through the snapshot's schema) or `declared` (entered by the contributor) | Realization and control models; claim provenance |
| Optional camera-control declarations | Camera and control-definition digests, reported firmware, selected or complete scope, and up to 64 typed confirmations/corrections within a 64 KiB canonical payload | Human confirmation of documentation-derived capabilities; separate from photo measurements and scientific credit |
| Monochrome controls | Original native sim and normalized family/filter (`none`, `yellow`, `red`, `green`), WC/MG toning where applicable, grain roughness and size; each applicable setting reproduced or declared | Developing and validating monochrome response, toning and uncertainty |
| Render classification and authorization | Signed `colour` or `monochrome`, separate from contribution kind and intent; explicit 180-day model-development authorization for monochrome | Analysis isolation and bounded deferred retention |
| Exposure | ISO, shutter, aperture, EC, as-shot WB multipliers | Nuisance priors, CCT |
| Capture date | **Month only** | Epoch sanity, drift analysis |
| Software tag | String | Edited-JPEG detection |
| Card render tag | Parsed fields only: format version, firmware id and version, look slug and version, 8-byte content-id prefix ([04 §8.4](04-matching-and-export.md)), plus a `render_tag_malformed` boolean for a recognizable card token that fails parsing; the raw `UserComment` string is dropped | Routing card renders out of every fit; per-firmware verification; keeping a malformed card token from becoming a stock render |
| Re-render group id | Random 16 bytes, client-generated per group | Server-verifiable grouping without a RAW hash |
| Body serial (optional, opt-out) | Sent in a dedicated request field; the server HMACs it on arrival and discards it ([05 §2](05-trust-and-integrity.md)) | Effective-contributor merging for caps only |
| Signature, nonce, proof-of-work | One signature and nonce per send; proof-of-work per photo batch | Integrity, anti-flood |

**Not uploaded:**
- RAW or content hashes.
- File names or paths.

### 1.1 Automatic error diagnostics

Production and local server-backed builds report `console.error`, uncaught exceptions, unhandled promise rejections and failures loading application scripts. The hooks run before WASM boot; prep, match and scene worker failures use the same channel. Serverless preview builds do not send reports.

`POST /api/client-errors` receives a version, closed error category and built-in error type, route category, deterministic client build SHA-256, and at most 12 application script/WASM locations. A location contains an allowlisted asset path and line/column; WASM uses function index and byte offset in those two fields. No free-form messages, console arguments, raw stacks, function names, page URLs, query strings, fragments, private status links, photo metadata, filenames, seeds or identity fields are included. The request omits credentials, cookies and referrer. The server stores its receipt time, not the client's clock or IP.

Diagnostics are JavaScript-generated failure locations, separate from the WASM-produced contribution envelope. They do not pass through the contribution inspector. The HAR verifier continues to flag diagnostic requests as unexpected; it does not certify this channel or silently allow its bytes as a contribution.

Each page attempts at most 20 reports, deduplicates repeated locations and makes no retry or persistent queue. Intake requires JSON, caps bodies at 16 KiB and shares a global in-memory bucket of 60 reports with one token replenished per second. Reports are untrusted client assertions. Private SQLite storage keeps at most 10,000 reports; inserts prune reports older than 14 days. Operator reads exclude expired reports without modifying the database. Expired rows on an idle service and database backups can remain until the next insert or backup expiry. There is no public read endpoint; [operator retrieval](ops/deployment.md#browser-error-diagnostics) requires host database access.

## 2. What is never uploaded

- Pixels: no thumbnails, previews or crops. RAW files embed full-size preview JPEGs; the client may use them **locally** (e.g. for coverage preselection, [07 §1](07-contribution.md)), and they're never sent.
- Pixel coordinates or spatial order.
- GPS/location, owner/artist/copyright, image unique IDs, face-detection data, raw makernote blobs.
- IP addresses are never stored. Rate limiting happens in memory only.

## 3. Credentials

- Passkeys (WebAuthn) only, created at the first contribution or when the contributor creates a profile in the sign-in dialog ([05 §2](05-trust-and-integrity.md), [08 §4.1](08-ui.md)). No email, no password, no account. Browsing and matching need no credential.
- **Contributor public keys and serial HMACs are never published.** Logs and changelogs use internal ids or display handles.
- **Serials:** the raw value exists only in a type that can be neither serialized nor printed with `Debug`; it's never stored or logged (a test scans the DB and logs). The HMAC is a stable identifier, which is part of why uploads count as pseudonymous personal data.

Upload envelopes are versioned at the wire boundary. Version 4 carries the send manifest with batch and attestation collections; a version 3 envelope is rejected before nonce consumption with `ENVELOPE_VERSION_UNSUPPORTED` and the action `reload_and_prepare`, so an old cached client never reuses a signature under the new layout. The previous live client ignores the response body and shows only `upload: HTTP 426`; it has no reload guidance. Deployment must require a reload and a fresh prepare. The current client also compares its expected module identity with the preparation module's version reply before the first operation; a cached page and module that disagree are refused with the same reload-and-prepare instruction.

## 4. Status, withdrawal and erasure

- **Status feed:** at first contribution, the client receives a random **capability token**. `/status/<token>` (and an RSS variant) shows accepted / down-weighted / quarantined per image. The server stores only the token's hash. The token isn't the public key. **Sign-in:** a passkey assertion from the same credential issues an additional token for that browser and leaves the credential's other tokens valid. Creating a profile before its first upload requires proof-of-work on that signed session request, at the same difficulty as a one-image upload by a new-tier credential; signing in with an existing credential does not. New profiles have tier `new` and no roles. **Sign out everywhere:** a signed request revokes every token of the credential. Each response to the credential's own sign-in names its roles, so the client can offer role-gated pages; roles are never published with the credential.
- **Session restoration:** `GET /api/session` with the status token as an `Authorization: Bearer` capability returns that profile's current roles, tier and display handle, with the same token. It creates no token and requires no new assertion. Missing, unknown or revoked tokens disclose no profile metadata.
- **Withdrawal:** `POST /api/withdraw` carries a nonempty `contribution_ids` list and one assertion from the contributing credential. Its withdrawal-domain digest commits the sorted, deduplicated canonical UUIDs joined by newline, without a trailing newline. Ownership is checked for the whole list and withdrawal is atomic; already-expired monochrome members do not prevent withdrawal of the remaining photos. Data is excluded from the next fit; blobs are hard-deleted after 30 days; published versions stay immutable.
- **Camera-control declaration withdrawal:** a canonical, passkey-authorized request names a documented camera and withdraws that owner's declarations. Existing sends cannot restore them after withdrawal; a fresh approval can submit new evidence. Withdrawn declarations are excluded from current agreement, while signed records and append-only audit history are retained. Withdrawal of a mixed photo send also withdraws the declaration occurrences associated with it.
- **Deferred monochrome:** contributing these complete measurements requires an explicit, unchecked-by-default selection for developing and validating monochrome response, toning and uncertainty. The same send approval covers this authorization. No published Look is promised. Measurements remain pseudonymous personal data, not anonymous data.
- Deferred measurements expire 180 days after authorization unless they have entered monochrome analysis or the contributor renews authorization. Expiry removes deferred sample blobs; withdrawal remains available. Blob deletion releases only the expiring contribution's references and preserves blobs still referenced by retained contributions. This is an engineering retention policy, not a legal-compliance determination.
- **Lost credential:** pending legal review ([10 Q3](10-open-questions.md)). One option is a server-keyed hash of each RAW that lets a user prove possession and delete. But that hash is itself a photo identifier, so it isn't implemented without a decision.

## 5. Client integrity and its limits

- The server validates blob structure, counts and ranges, and rejects any field outside the schema ([03 §2.2](03-data-model.md)).
- Subresource Integrity and published build hashes protect against a tampered CDN. They **can't** protect against a malicious origin, because the hashes come from the same origin. Say so on `/about/privacy`, publish reproducible-build hashes in the audit log, and **point privacy-sensitive users to the CLI**, which they can build from source. Sampling is random, so payloads differ between runs. To check that the web client sent only what it should:
  1. **Open devtools and turn on "Preserve log" before loading the site**, so requests sent on page load are recorded. Then export the **request body actually sent** (HAR export). Don't use the inspector's "Download payload", because a malicious client could offer a clean download while uploading something else.
  2. Export the local **seed file** from the Send step. It contains a seed per image or re-render group, plus the prior-transform version.
  3. Run `looklab verify-payload --seeds seeds.json --har upload.har --module looklab_prep_module.wasm --origin https://site.example --rp-id site.example --data data --kind library --intent native <files>`. Export the HAR with the network panel filtered to the upload session's API requests; any other request in it is flagged. Historical protocol-3 uploads require the matching historical CLI and prep module; the current protocol-4 CLI rejects protocol-3 bytes even with an older module. It runs the source-built prep module under wasmtime ([03 §2.1](03-data-model.md)), then:
     - **parses each upload body strictly** as the canonical envelope ([03 §2.3](03-data-model.md)) and rejects any deviation or trailing bytes;
     - groups the uploads by **send instance** (the nonce) and checks the whole send: one send manifest, one byte-identical assertion (a second assertion, even from the same credential, is reported as more than one passkey approval), and the send manifest's batch hashes exactly once each. A missing batch, an extra or duplicate upload, a batch absent from the manifest, and a mixed nonce are failures;
     - checks that **each upload's manifest and every sample blob reproduce byte for byte**. Each RAW and its included JPEGs are prepared once, jointly, with the seed file's seed; a RAW can feed several uploads, and each upload's images are selected from that prepared output by blob, metadata and re-render group, in order. Every included prepared image must appear in exactly one upload across the send. Monochrome renders are included only with an explicit model-development authorization matching the signed manifest. There's no tolerance, since a tolerance could hide data in low-order bits;
     - requires the **batch fields the files determine** to equal the manifest's: body from EXIF make/model against the `--data` snapshot, firmware from the JPEG's EXIF `Software` version (the RAW's firmware only when the JPEG carries none), sim and settings from the makernotes through the body's control schema. `--kind` and `--intent` supply what only you declare, and `--declare-setting key=step` (repeatable) supplies each control value you entered by hand; every batch field is printed with its source, and anything neither reproduced nor declared fails. A setting the files reproduce cannot also be declared, the declared set must match the manifest's `declared` entries, and the `--data` snapshot must contain the version the manifest names;
     - for opted-in monochrome measurements, requires `--monochrome-authorization model-development-180-days` to reproduce the signed purpose and retention authorization; original native sim, normalized family/filter and all applicable setting sources remain part of the check;
     - audits **manual camera-control declarations** as human-supplied evidence, not as fields reproduced from photographs. It checks the canonical request bytes, structure limits, manifest membership and shared assertion; prints the entire declaration with escaped strings and array order preserved; and marks it **review needed**. Review the firmware, selected controls and every corrected option. An attestation-only HAR needs no RAW/JPEG files or seed file, but still requires the source-built prep module. A withdrawal must name a locally documented camera and use canonical request bytes; its selected camera is displayed.
     - for **intent `card`**, reproduces the card declaration from the files' EXIF `UserComment` render tags; with no tag, `--firmware <id>/<version>` supplies what you declared, and it must equal the manifest's declaration;
     - checks that the remaining fields are the expected set:
       - **nonce:** 32 bytes, one per send;
       - **challenge:** the one inside `client_data_json`, which must equal H(nonce ‖ send manifest hash);
       - **WebAuthn fields**, checked **structurally**. No check relies on a key served by the site, because a malicious site could generate its own keypair in JavaScript and sign anything:
         - **`client_data_json`** (≤ 1024 bytes), parsed as JSON:
           - required: `type` = `webauthn.get`; `challenge` = the recomputed H(nonce ‖ send manifest hash); `origin` = the site's origin; `crossOrigin` false (absent counts as false);
           - accepted extras: `topOrigin`, and Chrome's `other_keys_can_be_added_here` with its fixed text. `topOrigin` must equal an expected top origin when one is declared, else it is printed for review;
           - the bytes must be the WebAuthn serialization (fixed key order, no whitespace, JSON string escapes). Any deviation is printed in full and marked **"review needed"**;
           - **any other key:** printed in full and the result marked **"review needed"** rather than pass/fail. Browsers may legitimately add keys, and data in a JSON key can't be hidden from a person reading it.
         - **`authenticator_data`**, fixed layout per the WebAuthn spec: `rpIdHash` = sha256 of the site's RP ID, then the flags byte and a 4-byte sign count. The attested-credential-data flag must be clear (an assertion carries none). If the extension flag is set, the extensions are decoded and printed for review.
         - **`credential_id`:** identical across every batch of a send and across all of your sends. verify-payload records it locally on first use (in its own state file, not anything the site provides) and compares on later runs, or across several HARs given at once.
         - **signature:** must be well-formed for a supported algorithm (Ed25519 64 bytes; ECDSA as valid DER; RSA at the key size). There's no fixed length rule. An optional check against the credential key, if you have it from your own records, is a sanity check for an honest site only; the guarantee doesn't rest on it.
       - **proof-of-work:** k solutions per batch, each verified against H(nonce ‖ batch manifest hash);
       - **serial:** equal to your files' EXIF serial, or absent;
     - lists **every request** in the HAR, flags any that aren't part of the expected API (nonce, upload, manual camera-control confirmation or withdrawal, status, passkey registration, signed session creation/sign-in and session restoration), and audits what was recorded. Signed session requests contain only the signed-request fields and optional proof-of-work, with a challenge bound to the session domain and empty payload. Session restoration takes no body or query, requires a 32-byte base64url bearer capability, and compares it with the upload response's status token when recorded. Registration bodies must match the client schema, including nested credential and response fields; client extensions are printed for review. A body on an endpoint that takes none, a registration key outside its schema, or a request header outside the browser's own set is reported.

     **Residual channels (documented, not closed):**
     - a client can choose *which* valid proof-of-work solutions it sends, leaking a few bits per batch at exponentially more work;
     - **the signature itself**: one signature per send, because every batch and confirmation must carry the same assertion; a site that forges keys can choose its bytes freely (tens to hundreds of bytes per send, depending on algorithm);
     - **the credential ID chosen at passkey registration**: a forged-key site picks it once (≤ 255 bytes, enforced at registration, a one-time leak; it's then fixed by the identical-ID check);
     - **human declarations:** firmware and corrected menu options are user-supplied text. Canonical encoding and signatures do not prove what those strings mean; the CLI exposes the complete declaration for human review rather than issuing an unconditional pass.
- timing.
  4. **When this guarantee holds:** only if the prep module uses no WASM relaxed-SIMD (whose results are implementation-defined across engines) and no NaN reaches stored values. Both are enforced in CI ([09 P1.3](09-roadmap.md)).

## 6. Publication

- **Per-image samples aren't published.** Only fitted targets, body parameters, control-response models and aggregate statistics are, under the data license ([10 Q2](10-open-questions.md)). This avoids linking pseudonymous credentials to photos posted elsewhere, and keeps withdrawal meaningful.
- Measurement use and image redistribution are separate permissions. Where measurement use is permitted, locally available RAW/JPEG pairs can supply private coordinate-free color measurements for fitting and comparisons without donating the source files. Record permission to use measurements separately from permission to publish each RAW or JPEG; measurement-only intake does not add images to public fixtures or previews.
- Fixture donation (for makernote mappings, [08 §4](08-ui.md)) is a separate, explicit, per-file opt-in. It **does** share the file, and the consent is recorded in the fixture manifest.
- The upload is signed over the file's name, size and sha256, with its own proof-of-work and a 64 MiB per-file cap. The server stores the bytes privately, keyed by sha256, and the proposal cites them as `committed = false` with a `source` naming that blob. A moderator approval exports the file into the community snapshot under `fixtures/donated/` and marks its manifest entry `committed = "lfs"`, so the donated RAW is stored in Git LFS rather than plain git.

## 7. Payload inspector (required before every upload)

1. **Color cloud:** a 3D scatter of exactly what leaves.
2. **"Can you see the photo?":** samples laid out as an image in upload order, which looks like noise.
3. **Size:** e.g. "41.2 MB → 380 KB", per image and in total.
4. **Metadata table:** every field sent, plus removed fields with their values redacted. The serial row says plainly: "sent once, converted to a keyed hash on arrival, never stored", with an opt-out toggle. The table includes rendering classification, native sim, normalized settings with sources and monochrome purpose/retention authorization. It adds no spatial data, source-file hashes, raw makernote blobs or sampling seeds.
5. **Download payload** (a convenience view with a JSON rendering, not a trust anchor; verify against the HAR, §5) at the inspector, and **export seed file** at the Send step (local only, never uploaded, labeled sensitive; [03 §2.1](03-data-model.md)).
6. **Plain statement:** samples can't be turned back into a photo, but could be matched by someone who has it. No account or IP is stored.
7. **Source link:** the client code (AGPL) and the CLI alternative.

## 8. Later: public galleries

Post-launch: a separate, explicit, per-image opt-in with downscaled uploads and its own moderation. Launch previews use canonical images owned by the project ([08 §5](08-ui.md)).
