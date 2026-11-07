# Manual passkey and HAR checks

P5.4 and P5.7 need a real browser prompt, a phone or a security key in a few places that no headless engine can supply. This page covers those checks and states the expected result for each. Everything else in P5.4 runs automatically; [Automated coverage](#automated-coverage) lists what, so nobody repeats it by hand.

## Automated coverage

`site/checks/e2e.mjs --flow contribute` drives the whole contribute flow (drop the fixture pair, prepare, review, create a profile, sign, upload, open the status feed), records a HAR of the API requests, exports the seed file and runs `verify-payload` on the recorded HAR. Chromium uses a CDP virtual authenticator; Firefox and WebKit use Playwright's context credentials. All three answer WebAuthn create and get with a discoverable, user-verified passkey.

```sh
# one engine at a time, with the stub site from site/scripts/dev.sh on 8081
nix develop -c env \
  PLAYWRIGHT_MODULE="$(nix eval --raw nixpkgs#playwright-driver)/index.mjs" \
  PLAYWRIGHT_BROWSERS_PATH="$(nix eval --raw nixpkgs#playwright-driver.browsers)" \
  node site/checks/e2e.mjs --browser chromium --flow contribute \
    --origin http://localhost:8081 --out /tmp/looklab-e2e/chromium \
    --state target-ui/stub-state --verifier target/debug/looklab
```

`--browser firefox` and `--browser webkit` run the same flow. Add `--fail-draft-blobs 1` to a Chromium run to force every draft `Blob` write to fail; the run then asserts the draft-unavailable note before continuing. On a machine without a GPU, the Playwright WPE WebKit build needs Mesa's software EGL:

```sh
__EGL_VENDOR_LIBRARY_FILENAMES=<mesa>/share/glvnd/egl_vendor.d/50_mesa.json \
EGL_PLATFORM=surfaceless MESA_LOADER_DRIVER_OVERRIDE=swrast \
LIBGL_DRIVERS_PATH=<mesa>/lib/dri LIBGL_ALWAYS_SOFTWARE=1 \
  node site/checks/e2e.mjs --browser webkit --flow contribute ...
```

Results on 2026-11-07, Playwright 1.64.1, a private stub instance on port 8094:

| Engine | Version | Passkey create and batch signing | HARs | `verify-payload` |
|---|---|---|---|---|
| Chromium | HeadlessChrome 156 | CDP virtual authenticator (CTAP2, resident, UV) | desktop and mobile | PASS |
| Firefox | 157.0 | context credentials (resident, UV) | desktop and mobile | PASS |
| WebKit | 27.2 (WPE) | context credentials (resident, UV) | desktop and mobile | PASS |

WebKit's IndexedDB refuses to store `Blob` values in this build (`UnknownError: Error preparing Blob/File data to be stored in object store`), so the local draft's copy of the selected files is not saved. The contribute flow reports one non-blocking note, **This browser couldn't keep a local draft, so these photos won't be restored after a reload. Processing and sending still work.**, and continues; the upload and its HAR verify as on the other engines. The draft store must never let a failed write block later ones: WebKit does not abort a transaction that fails after `store.clear()`, so the clear runs in its own transaction and a failed transaction is aborted explicitly.

Chromium reproduces the failure path without a WebKit build: `--fail-draft-blobs 1` makes every IndexedDB `Blob` put throw, and the flow asserts the note, completes the upload and passes `verify-payload`.

The `/help/passkeys` guidance page renders in all three engines with the heading **Sign in without a password** and the sections **How it works**, **Use your phone**, **Security keys and password managers**, **On Linux**, **If a prompt doesn't finish** and **What's stored**; a direct navigation check confirmed this in each engine.

### Injected variants

All 40 tests in `crates/looklab-cli/tests/verify_payload.rs` pass. The prep module must be built in the same captured lane first:

```sh
nix develop -c tools/build.sh --lane native -- \
  sh -c 'sh site/scripts/wasm-assets.sh --prep && cargo test --locked -p looklab-cli --test verify_payload'
```

Each named variant maps to a test:

| Criterion | Test |
|---|---|
| extra field | `fails_on_an_extra_manifest_field`, `fails_on_a_session_body_with_an_extra_field` |
| reordered field | `fails_on_a_reordered_field` |
| trailing bytes | `fails_on_trailing_bytes` |
| altered low-order sample bits | `fails_on_altered_low_order_sample_bits` |
| unexpected extra request | `fails_on_an_unexpected_request` |
| wrong challenge | `fails_on_a_wrong_challenge` |
| wrong origin | `fails_on_a_wrong_origin` |
| wrong rpIdHash | `fails_on_a_wrong_rp_id_hash` |
| changed credential ID | `fails_when_the_credential_id_changes_between_two_hars`, `fails_when_the_credential_id_changes_between_runs` |
| unknown `client_data_json` key | `marks_an_unknown_client_data_key_for_review`, `marks_an_unexpected_top_origin_for_review`, `marks_a_non_canonical_client_data_json_for_review` |
| authenticator extensions | `marks_authenticator_extensions_for_review` |

The same failures were also observed against a browser-produced HAR: a state file holding another credential id fails with `the credential id ... differs from ...`, an appended `POST /api/collect` fails with `unexpected request in ...`, and `--origin http://evil.test` fails because no request in the HAR belongs to that origin.

## What is manual

Every row below also needs the HAR capture and a `PASS` from `verify-payload`.

| Environment | Paths to check |
|---|---|
| Chrome or Chromium, Linux desktop | phone QR, security key |
| Firefox, Linux desktop | security key |
| Firefox, Windows or macOS | platform passkey (Windows Hello, Touch ID), security key |
| Safari, macOS 13+ or iOS 16+ | platform passkey (Touch ID, Face ID), security key, a passkey synced through iCloud Keychain |

## Prerequisites

1. A checkout whose site data generation succeeds. `site/scripts/dev.sh` regenerates it; on a commit whose fit does not converge the launcher stops before serving.
2. `nix develop` in the repository root.
3. The fixtures:

   ```sh
   tools/fetch-fixtures.sh
   ```

   This downloads `fixtures/cache/fujifilm-x-s10.raf`; `fixtures/generated/fujifilm-x-s10.jpg` is committed.
4. The verifier and the prep module:

   ```sh
   nix develop -c tools/build.sh --lane native -- cargo build --locked -p looklab-cli
   nix develop -c sh site/scripts/wasm-assets.sh --prep
   ```

5. The stub site, in a second terminal:

   ```sh
   site/scripts/dev.sh
   ```

   It serves `http://localhost:8081` (proxied to the stub on 8098) with the stub nonce, stub proof of work and the real server's passkey routes. Stop any other process using either port first.

Keep the seed file, the HAR and the verify state under a private directory, for example `~/looklab-checks`. The seed file and the HAR describe your photographs.

## Check 1: Chrome or Chromium on a Linux desktop, phone QR

1. Open DevTools, Network, enable **Preserve log** (see [Capturing the HAR](#capturing-the-har)), then open `http://localhost:8081/contribute`.
2. Choose **Share photos you already have**, then **Choose photos**, and select the RAW and its JPEG.
3. Set body `fujifilm/x-s10`, firmware `1.00`, simulation `provia`. Leave the technical settings editor empty.
4. Choose **Process locally** and wait for **Review what**.
5. Choose **Export seed file** and save it as `seeds-chrome-qr.json`.
6. Choose **Continue to send**, then **Send contribution**. In the dialog, leave the display name empty and choose **Create a profile**.
7. In the Chrome passkey prompt choose **Use a phone or tablet**. Scan the QR code with the phone and approve with the phone's screen lock.
8. Expect **Thanks for helping build looks**, then **Your contributions** listing the batch and its one image.
9. Capture the HAR and run `verify-payload` as described below, with `--state state-chrome-qr.json`. Expect `PASS`.
10. Sign out, then **Continue with passkey** and approve on the phone. Expect the account menu and `/me` without a new upload.

## Check 2: Chrome or Chromium on a Linux desktop, security key

Repeat Check 1, choosing the security-key option in the browser prompt instead of the phone, and use `--state state-chrome-key.json`. The key must hold a discoverable credential and support user verification (PIN or biometric): the server requests a resident key with `user_verification: required`, and a key that cannot do both fails at registration.

Expect the same pages and `PASS`.

## Check 3: Firefox

Firefox on Linux has no platform passkey store; use the security-key path. On Windows and macOS, Firefox offers Windows Hello or Touch ID instead, and that path is also required.

1. Open DevTools, Network, enable **Persist Logs**, then repeat Check 1 steps 2 to 6.
2. In the Firefox prompt choose the platform authenticator or the security key and approve.
3. Expect the thanks page and the contribution feed entry.
4. Capture the HAR, verify with `--state state-firefox.json`, expect `PASS`.

## Check 4: Safari

On macOS 13 or later with iCloud Keychain enabled, or iOS 16 or later:

1. Open Web Inspector, Network, enable **Preserve Log**, then repeat Check 1 steps 2 to 6 in Safari.
2. Approve with Touch ID or Face ID. A passkey synced through iCloud Keychain works on another Apple device signed into the same account, without a QR code.
3. Expect the thanks page and the contribution feed entry.
4. Capture the HAR with Web Inspector and verify with `--state state-safari.json`, expect `PASS`.
5. Repeat once with a security key, using a separate state file.

## Capturing the HAR

Start the network recording before the page load so the session requests are included.

- **Chrome or Chromium:** DevTools, Network, enable **Preserve log**. After the upload, right-click the request list and choose **Save all as HAR with content**.
- **Firefox:** DevTools, Network, enable **Persist Logs**, then **Save All As HAR**.
- **Safari:** Web Inspector, Network, enable **Preserve Log**, then **Export**.

All three save every recorded request. Delete the entries whose URL is not `http://localhost:8081/api/...` or `http://localhost:8081/status/...`, and do not edit an API entry. Any other request left in the file is reported as a failure by `verify-payload`, so a stray static asset is easy to spot.

## Running verify-payload

```sh
target/debug/looklab verify-payload \
  --seeds ~/looklab-checks/seeds-chrome-qr.json \
  --har ~/looklab-checks/chrome-qr.har \
  --module site/generated/wasm/looklab_prep_module.wasm \
  --origin http://localhost:8081 \
  --rp-id localhost \
  --data data \
  --kind library \
  --intent native \
  --pow-memory-kib 8 --pow-iterations 1 --pow-difficulty 0 \
  --state ~/looklab-checks/state-chrome-qr.json \
  fixtures/cache/fujifilm-x-s10.raf fixtures/generated/fujifilm-x-s10.jpg
```

The three `--pow-*` values match the stub endpoint. Against a production server, omit them and pass the server's values only if they differ from the defaults.

Expected output: the list of requests in the HAR, each marked `[expected]`, the reproduced batch fields with their source, the credential id, then `PASS` and exit status 0. The credential id line says `recorded ... on first use` for a new state file and `matches the value ... recorded earlier` on a repeat run with the same passkey.

Use one state file per passkey. Two different browser profiles create two different credentials, and comparing them against one state file fails with `the credential id ... differs from ...`. That failure is the intended check, not a browser defect.

`REVIEW NEEDED` is a separate result: the browser sent a `client_data_json` key or an authenticator extension that `verify-payload` does not know. Record the printed lines and the browser version; do not count the run as a pass. `FAIL` prints each reason; record them with the HAR.

## Recording results

Add one row per run to the P5.4 or P5.7 evidence table:

| Field | Value |
|---|---|
| Browser and version | from `about:version` or the browser menu |
| Operating system | distribution and version |
| Passkey path | platform, phone QR, security key or password manager |
| HAR and seed file | paths, kept private |
| `verify-payload` result | `PASS`, `REVIEW NEEDED` or `FAIL` with the printed reasons |
| Notes | prompt wording, which device approved, any retry |
