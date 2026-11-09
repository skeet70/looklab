# 09 — Roadmap

Phases are ordered by dependency. Each task has an ID, the docs it implements and acceptance criteria (AC). Tasks in a phase can run in parallel unless they list `deps`. Thresholds marked *(tentative)* should be revisited once real data exists. Record any change in the doc and in the PR.

## Phase 0: Foundations

| ID | Task | Docs | AC |
|---|---|---|---|
| P0.1 | Cargo workspace per [01](01-architecture.md); CI: fmt, clippy `-D warnings` with `disallowed_methods` banning all `f64`/`f32` transcendental methods in core crates plus the float-method allow-list check (AGENTS.md), tests, **wasm32 build of all core crates including `looklab-raw`** (rawler with `uuid/js`); SPDX headers; `NOTICE` | 01, 02 §8 | CI green; wasm32 build passes for `color`, `samples`, `model`, `fit`, `match`, `export`, `trust`, `prep`, `meta`, `raw`, `schema`, `report`, `wasm`; a wasm32 test decodes one fixture RAF and one DNG under Node with output identical to native |
| P0.2 | `tools/golden/`: pinned colour-science scripts generating vectors for P0.3. CAM16 vectors use `discount_illuminant=True` at the reference conditions | 02 §8 | Vectors committed; regeneration documented; deterministic |
| P0.3 | `looklab-color` (all transcendentals via `libm`): XYZ/xyY, sRGB/Adobe RGB/Rec.2020/P3/ProPhoto + TRCs, CAT02/CAT16/Bradford, CIELAB, CIEDE2000, CAM16 fwd/inv, CAM16-UCS/LCD/SCD, ΔE in each; chart data | 02 | Golden within 1e-6 relative (CAM16 inverse 1e-5); round-trips; **native and wasm32 outputs bit-identical** on 10⁵ random inputs per function |
| P0.4 | `looklab-samples`: blob v1 with enums, `n` cap, header and decompression limits | 03 §2.2 | Round-trip; cargo-fuzz reader; malformed, oversized and zip-bomb inputs rejected |
| P0.5 | Fixtures: owner's body, 20+ RAW+JPEG pairs across ≥ 3 sims, incl. in-camera re-renders at **non-default settings**, 1 chart session, 1 gradient. Manifest + checksums + consent; small CI subset | 07 | Manifest + fetch script; public CC0 RAWs fetched by checksum, donated RAWs in Git LFS, only test vectors below 1 MB in plain git |

## Phase 1: Single-body CLI (fit, compare, export)

| ID | Task | Deps | AC |
|---|---|---|---|
| P1.1 | `looklab-raw`: rawler decode → linear camera RGB, levels, as-shot WB, orientation; decoder trait | P0.1 | All fixtures decode; within 0.5% of a reference decode |
| P1.2 | `looklab-meta`: EXIF + makernotes for the owner's make; sim/settings extraction; whitelist sanitizer | P0.1 | Correct on all fixtures; property test: output has only whitelisted keys |
| P1.3 | `looklab-prep`: pairing, alignment (similarity + radial distortion + **radial gain**), downsample, masks, density sampling, **shared sample set for re-render groups**, shuffle; coverage-gain preselection from embedded previews | P1.1, P1.2, P0.4 | Alignment residual < 0.3 px; on a fixture with in-camera shading correction, luminance residual vs radius is flat within 1% after the gain fit; re-render group members have identical `in_rgb`; no coordinates in output (by type); production seeds are fresh CSPRNG (test: seed source isn't reachable from any server-provided value); prep builds as the `looklab-wasm` prep module, and `looklab-cli verify-payload` under wasmtime reproduces its output byte for byte from a seed file, on 2 host platforms; **CI rejects the module if it enables relaxed-SIMD** (target-feature check + `wasm-tools validate` without it) and fuzzes prep for NaN in outputs |
| P1.4 | `looklab-model`: layers T, N, G/R/Λ (colorimetric-reference formulation with the exact `Y_ref` solve), soft clip, residual; evaluation; look file v1; probe set v1 | P0.3 | Evaluation < 3 µs/color native *(tentative)*; file round-trip exact; **identity test**: a colorimetric Look (T = colorimetric tone, N = 0, G = 1, R = Λ = 0) reproduces colorimetric CAM16-UCS on all probes, and neutral probes give a′b′ = 0 |
| P1.5 | `looklab-fit`: single-body staged LM, Student-t likelihood with σ_v (no weight duplication), tempering, nuisance + body realization params, per-stage GCV for λ, image-clustered sandwich uncertainty, outlier scoring | P1.3, P1.4 | Held-out scene colors against the **latent mean Look response**: median ΔE₍UCS₎ ≤ 1.5, p95 ≤ 4.0 *(tentative)*; a mislabeled image gets z > 3.5; **20 images < 60 s native**; noise-free, adequately sampled pure-tone identifiability with normal priors and converged fits: log-G difference < 0.02, rotation < 1.8°, lightness < 2.0 J′; retain separate noisy latent-recovery, uncertainty-coverage and supported-bias/outlier evidence |
| P1.6 | `looklab-report`: SVG/HTML report for a Look and for a comparison; natural-language summary | P1.4 | Correct dominant differences on synthetic injected deltas |
| P1.7 | `looklab-export`: `.cube` (+ shaper) for `camera_rgb_wb_linear`, single-illuminant; pipeline simulator; validation report incl. other-CCT errors | P1.4 | Round-trip p95 ≤ 1.0 ΔE on supported probes at the bake CCT |
| P1.8 | `looklab-cli`: `prep`, `fit`, `report`, `compare`, `export`, `validate` | P1.3–P1.7 | End to end on fixtures |
| **P1.9** | **Premise spike (G1):** rent a second body that shares a sim with the owner's; chart-anchor both; fit the sim independently on each; compare on supported probes | P1.8 | Report written. **Decision gate:** if median cross-body ΔE > 2.0 after input transforms *(tentative)*, revisit the shared-θ design (e.g. per-body realizations with a looser family prior) before Phase 4 |

**Milestone M1:** the owner can fit, compare and export every native sim on their body and their own firmware's sim. P1.9 tells us whether Looks generalize across sensors.

## Phase 2: Matching on one body

| ID | Task | Deps | AC |
|---|---|---|---|
| P2.1 | Control schema format + `looklab-schema` validator (native + WASM); owner's body schema with verified mappings | P1.2 | Validates; mappings fixture-backed; example-schema rules enforced (types, `affects` vocabulary, declared controls only) |
| P2.2 | **Joint θ/D fit** from all native data (default + non-default), WB/exposure controls as input effects, body ξ/ω | P1.5, P2.1 | Predicts held-out **observed JPEG re-renders**, including their observation noise, at unseen settings within 1.5 ΔE median *(tentative)*; adding non-default library images measurably reduces θ uncertainty |
| P2.3 | Settings optimizer: pruning, probe subset, per-probe linearization, low-rank covariance, staged coordinate descent + perturbation; settings card incl. EC/WB; unreachable regions | P2.2 | Targets = **held-out re-renders at non-default settings** (fitted realization): recovered settings within 1 step per control, or predicted ΔE within 0.5 of the true settings' ΔE; **full search < 2 s in one wasm32 worker** (p95 over fixtures) |
| P2.4 | CLI `match`, `verify` | P2.3 | The loop reduces measured error ≥ 30% vs. the first suggestion when the model is deliberately degraded |

**Milestone M2:** recommended, verified settings on the owner's body.

## Phase 3: Firmware and pipeline interfaces

| ID | Task | Deps | AC |
|---|---|---|---|
| P3.1 | Software interface profile schema; all insertion points; `illuminant_handling` single/dual; downstream inversion with perceptual clipping | P1.7 | Synthetic pipelines: round-trip p95 ≤ 1.0 ΔE where invertible, at each bake CCT |
| P3.2 | Card format ([04 §8](04-matching-and-export.md)): deterministic writer, strict reader and normative grid evaluator in `looklab-export` (native + wasm32); `looklab-cli card inspect \| eval`; `card.llk` in every version manifest | P1.4 | Same Look version → same bytes; reader rejects every malformed file in the conformance set; native and wasm32 evaluation bit-identical; `NEUTRAL` and `FIT` outputs equal direct Look evaluation |
| P3.3 | Writers: HaldCLUT, `native_json` | P3.1 | Round-trip tests |
| P3.4 | Card conformance vectors in `data/card/conformance-v1/` (valid and malformed files, grid-evaluation vectors, render-tag strings) | P3.2 | Reference reader and evaluator pass all vectors; regeneration documented |
| P3.5 | Render tag: `looklab-meta` parses `UserComment` into typed fields; sanitizer whitelists them ([06 §1](06-privacy-and-payload.md)); client sets intent `card` and checks `cid` against the published file; worker excludes intent `card` from every fit and aggregates per-firmware verification | P1.2, P3.2 | Property test: only the parsed fields leave; a tagged synthetic render never changes any Look or body parameter; a `cid` mismatch is excluded and reported |
| P3.6 | Example translator: portable C99, no allocator, fits a 3×3 matrix and per-channel tone curve from `FIT` and `NEUTRAL` | P3.2 | On synthetic matrix + curve looks, recovers the injected parameters; builds with `-std=c99 -Wall -Werror` and no libc allocation |

## Phase 4: Multi-body

| ID | Task | Deps | AC |
|---|---|---|---|
| P4.1 | Input transform priors: dual-illuminant DNG matrices, database matrices, SSF import; CCT estimation from multipliers; chart anchor term | P1.5 | Prior chart ΔE reported per source; anchoring reduces chart ΔE; CCT within 150 K of reference on fixtures *(tentative)* |
| P4.2 | Global hierarchical fit: block coordinate descent (bodies ↔ Looks), global gauge from anchored bodies, per-firmware offsets, σ_v; refit cascade with dirty-body batching; provenance versions | P4.1, P2.2 | Synthetic bodies: known Δ_c, ξ, ω recovered within prior σ; Look within 0.5 ΔE median; one body update triggers exactly the dependent Look refits; the 1k-image refit converges within the time bound, and a non-converged batch publishes nothing and resumes from its stored state |
| P4.3 | Uncertainty: bootstrap (B = 32), sandwich, leave-one-body-out; support marking | P4.2 | On held-out bodies, true error inside the 90% band ≥ 85% of the time *(tentative)*; synthetic out-of-gamut targets are marked unsupported |
| P4.4 | Epoch + look split detection (image-level permutation T²) | P4.2 | Synthetic silent change detected; no false split on same-version data at 10⁶ samples |

## Phase 5: Browser client (anonymous)

| ID | Task | Deps | AC |
|---|---|---|---|
| P5.1 | `looklab-wasm` + **N single-threaded workers** (no SharedArrayBuffer, no COOP/COEP); one file per worker at a time | P1.3 | 60 MP RAW processed < 10 s on a mid-range laptop *(tentative)*; **≤ 1 GB per worker and ≤ 2 GB total** across workers, measured on a 100-file folder (worker recycling must keep it flat); N chosen by the budget rule in 01 |
| P5.2 | `looklab-web` static site: bodies, looks, compare; artifacts checked by hash against version manifests | P1.6 | Renders from static data; mobile-responsive |
| P5.3 | Match + verify in browser; permalinks pin all versions | P2.3, P5.1 | Same results as the CLI (bit-identical evaluation); permalinks reproduce |
| P5.4 | Payload inspector, seed-file export, canonical upload envelope, `verify-payload` HAR flow | P5.1, P1.3 | Against a **local stub endpoint** (stub nonce, stub proof-of-work, real WebAuthn on test credentials): `verify-payload` passes on the HAR from Chrome, Safari and Firefox; it fails on injected variants (extra field, reordered field, trailing bytes, altered low-order sample bits, unexpected extra request, wrong challenge/origin/rpIdHash, changed credential ID) and marks **review needed** for unknown `client_data_json` keys or authenticator extensions, **without consulting any site-served key** |
| P5.5 | Canonical scene previews | P1.4 | < 200 ms per look switch *(tentative)* |
| P5.6 | Phone pattern page (seeded) + detector | P1.3 | Detection ≥ 95% on 3 phones × 3 exposures |
| P5.7 | Credential UX: passkey creation at first contribution, batch signing, cross-device (QR) and security-key paths, guidance for platforms without a passkey store, status link | P5.1 | Works in Chrome, Safari, Firefox, including a Linux desktop via phone QR flow and via security key |
| P5.8 | Fast UI dev loop: debug `site/scripts/build-site.sh` build with the stub server behind a proxy, reused worker wasm, a dedicated target dir | P5.2 | An edit to a page or `site/styles.css` is visible in the browser within 60 s *(tentative)*; documented in README |
| P5.9 | UX foundations ([08 §0, §1, §7](08-ui.md)): achromatic design tokens (dark and light), header, account menu and footer, the routes of 08 §1.1 (old routes removed), the vocabulary of 08 §0.3 on every page, the shared components of 08 §7.2 | P5.2 | An e2e text scan finds no right-column term of 08 §0.3 outside inspect views; every neutral color in the stylesheet has R = G = B; keyboard-only walkthrough of navigation and dialogs; 375 px wide pages have no horizontal scroll |
| P5.10 | Sample photos ([08 §5](08-ui.md)): CC0 photo scenes with recorded source, license and sha256, thumbnail variants | P5.5 | 4 to 6 photo scenes published by `site_data`; look switch < 200 ms at viewer size *(tentative)* |
| P5.11 | Viewer and Compare ([08 §2.3, §7.2](08-ui.md)) | P5.9, P5.10 | Split, side by side, toggle and difference views; URL state round-trips; the divider works by mouse, touch and keyboard; summary sentence and five-row breakdown; measurements behind a disclosure |
| P5.12 | Home, Looks, Look, Cameras and Camera pages with three depths ([08 §2](08-ui.md)) | P5.9, P5.10 | The glance level shows an image and a sentence before any plot or number; diagnostics and provenance sit in the Measurements and History tabs; recipe leaderboards on look and camera pages |
| P5.13 | "Get the look" wizard and result ([08 §3](08-ui.md)) | P5.3, P5.9 | Camera and look prefilled from the URL land on the result; target vs. realization split view; closeness label with numbers on demand; verification inline; permalink reproduces |
| P5.14 | Sign-in and Your contributions ([08 §4.1](08-ui.md), [06 §4](06-privacy-and-payload.md)): server `POST /api/session` (signed assertion → an additional status token plus the credential's own roles) and `POST /api/session/revoke` (revokes every token of the credential), replacing `/api/status/reissue`; sign-in dialog, account menu, `/me`, `/status/:token` as sign-in | P5.7, P6.1 | Signing in from two browsers leaves both tokens valid; sign out everywhere revokes both; roles reach only the credential's own session; the no-IP and no-public-key scanning tests still pass; e2e sign-in with a virtual authenticator |
| P5.15 | Contribute hub, flow and wanted shots ([08 §4.2](08-ui.md)) | P5.9, P6.4 | Ways-to-help cards; drop zone; stepper; the one-line payload summary expands into the full inspector; the e2e upload flow still passes |
| P5.16 | Developer pages ([08 §8](08-ui.md)): card inspector and render-tag checker in the browser | P3.2, P3.5, P5.9 | The inspector reports the expected outcome for every file in `data/card/conformance-v1/`; the checker parses every conformance render-tag string as expected |
| P5.17 | Community hub, propose picker and review area ([08 §4.3, §6, §9](08-ui.md)) | P5.14, P6.5 | No moderation link for visitors; Review reachable from the account menu for a moderator credential; the decision dialog previews the audit-log line; the e2e proposal → decision flow still passes |

## Phase 6: Server and crowdsourcing

| ID | Task | Deps | AC |
|---|---|---|---|
| P6.1 | `looklab-server` on SQLite (WAL) + Litestream: nonce, WebAuthn (`webauthn-rs`) batch-signature verification, memory-hard PoW (cost ∝ images, difficulty by tier/reputation), schema validation, serial HMAC on arrival, status tokens, signed withdrawal | P0.4 | Invalid signatures, PoW or schema rejected; scanning test: **no IP addresses or raw serials anywhere** (DB, logs, published outputs), **no public keys or serial HMACs in logs or published outputs**, and public keys in the DB only in `Credential.webauthn_credential`; Litestream restore drill documented |
| P6.2 | `looklab-worker`: `jobs` table polling, consistency scoring, effective-contributor merging, honest-duplicate merging, nightly batches | P4.2, P6.1 | Fit-free and budget-short-circuit worker checks pass in CI; real fitter/publication integration passes before releases; duplicates from one effective contributor merged; maintainer-requested 1k-image Look refit < 15 min *(tentative)* |
| P6.3 | `looklab-trust`: weights (kind × evidence × tier × reputation), reputation updates, new-tier group cap (Look regions + body parameters), per-effective-contributor caps with minimum pool and serial cap order, release gates vs previous + anchor, re-anchor and revert/exclude/refit | P6.2 | **Targeted trust-policy tests pass in CI; compact real fitter-to-trust integration passes before releases (ordinary CI only if measured job wall time is approximately ≤ 2 min); full-scale harness runs only on explicit maintainer request ([05 §10](05-trust-and-integrity.md))** |
| P6.4 | Contribution UI (ladder, preselection, coverage map, inspector, status) | P5.x, P6.1 | Full flow on staging with fixtures; **`verify-payload` passes on staging HARs** with real nonces, proof-of-work and signatures |
| P6.5 | Proposals + moderation in app: menu-level summaries, mapping inference from labeled files, WASM validator, confirmations as evidence, fixture donation, LUT license declaration, interface submissions with round-trip report, signed decisions, community data snapshots validated in CI | P6.1, P2.1 | End-to-end proposal → decision → snapshot → CI validation |
| P6.6 | Tiers and roles: `looklab-cli admin` + admin page for grants, seeder endorsement and root-admission flows, nomination queue from reputation, append-only audit log published as page + JSON, version manifests with artifact hashes | P6.3 | Grant/endorse/admit/revoke each produce a row + log entry and appear in public lists; revoking a root admission produces a new version and marks dependents; clients reject artifacts whose hash doesn't match the manifest |
| P6.7 | Bounties + recipe scores + leaderboards | P6.3, P2.3 | Bounties on synthetic recipe-residual data point at the injected region; bounty regions not resolvable by new-tier data alone |
| P6.8 | Deferred-monochrome intake, distinct from contribution kind and intent, with explicit model-development authorization and 180-day retention ([03 §1, §2](03-data-model.md), [05 §5](05-trust-and-integrity.md), [06 §4](06-privacy-and-payload.md), [07 §2](07-contribution.md), [08 §4.2](08-ui.md)) | P5.4, P6.1, P6.4 | Mixed colour/Acros/Sepia RAW group preserves byte-identical `in_rgb`, toned output channels and declared colour space; one approval covers every batch and `verify-payload` reproduces every included render; authoritative classification blocks mislabeled batches from all colour fit/gauge/support/release/reputation paths; expiry and withdrawal delete deferred blobs without deleting retained shared references; inspector adds no spatial data, source-file hashes, raw makernotes or seeds |
| P6.9 | Documentation-derived camera capabilities and owner confirmation: a representative camera list from the last fifteen years, weighted toward Fujifilm then Olympus/OM System; official manual/firmware citations; a validated non-authoritative catalog; source-labeled camera UI and signed uploader confirmations/corrections | P5.12, P6.4, P6.5 | Documentation-only records are visibly human-unconfirmed and distinguish capabilities from decoder/contribution/fit support. Confirmation is scoped to camera, firmware and controls, deduplicates identity groups, retains contradictions and supports audited moderator evidence. Sufficient independent agreement can confirm the documentation record but grants no contributor trust or scientific credit and bypasses no authority-schema approval. |

**Launch = M1–M2 + P1.9 passed + P3.1–P3.2 + P3.4–P3.5 + P4 + P5 + P6.1–P6.6.** P3.6 and P6.7 can follow shortly after.

## Phase 7: Post-launch

darktable / RawTherapee as software bodies (`.dtstyle`); HEIF; C2PA verification; automatic chart detection; `fixed_illuminant` looks; separate per-illuminant Δ and root-polynomial input transforms; spatial-effect measurement; opt-in galleries.
