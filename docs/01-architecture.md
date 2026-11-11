# 01 — Architecture

## System shape

```
┌──────────────────────────── Browser (WASM) ────────────────────────────┐
│ looklab-web (UI)                                                       │
│   └─ N single-threaded web workers ─ looklab-wasm                      │
│        ├─ raw decode (looklab-raw)       ├─ match/optimize (local)     │
│        ├─ metadata + sanitize (meta)     ├─ preview render (model)     │
│        ├─ align/mask/sample (prep)       ├─ schema validator           │
│        └─ chart/pattern detect (prep)    └─ payload inspector          │
│   credential: passkey (WebAuthn), only when contributing               │
│                     │ signed batches: samples + sanitized metadata     │
└─────────────────────┼──────────────────────────────────────────────────┘
                      ▼ HTTPS
┌──────────────────────────── Server ────────────────────────────────────┐
│ looklab-server (axum): upload intake, signed-request endpoints         │
│   (status, withdrawal), proposals + moderation, grants/endorsements    │
│   SQLite (+ Litestream replication) ── entities, jobs, audit log       │
│   Object store ── sample blobs (private), fit artifacts (by hash)      │
│ looklab-worker: fits, trust scoring, release gates, bounties,          │
│   community-data snapshots, static-site publish                        │
└───────────────────────────┬────────────────────────────────────────────┘
                            ▼ publish
              Static site / CDN: looks, bodies, previews, audit log (page +
              JSON), artifacts by hash (clients check version manifests)
```

**Division of work**

- **Client:** everything that touches pixels. That means decode, alignment, masking, sampling, detection, sanitization and the payload inspector. Also local matching, verification, previews and proposal validation. Anonymous use is the default. A credential is created only on first contribution.
- **Server:** intake, shared fits, trust and release gates, moderation, the audit log and publishing.
- **Reads:** a static site served from a CDN. There are no user sessions for contributors, no OAuth and no email. Moderators authenticate per action, or with a short-lived session opened by a passkey assertion.
- **Client claims are bounded, not verified.** Contribution kinds are client-claimed. New-tier data is capped, so false claims have bounded effect ([05 §1](05-trust-and-integrity.md)).

The **CLI** (`looklab-cli`) runs the same core crates natively. It's the Phase 1 deliverable, the dev and power-user tool, and the recommended path for privacy-sensitive users who don't want to trust a served client ([06 §5](06-privacy-and-payload.md)).

### Shared targets and camera realizations

One common model/evaluator/fitter serves all bodies. A native-family target has a stable family identity and immutable coefficient versions. Camera input calibration precedes that Look in the forward model; body realization, firmware and control-response parameters describe its camera rendering ([02 §§1–2, §4](02-color-pipeline.md)). They are learned data, not separate camera algorithms or canonical targets.

Camera-realization revisions are scoped by `(body, epoch, native_sim)` and pin exact target, input-transform and native/recipe parameter/CRM artifacts. Separate their attempts, histories and current selections from family-target revisions ([03 §1](03-data-model.md#1-entities)). New target versions schedule dependent realization work; they do not retarget old models through a latest-version lookup.

Eligible evidence on any supported body may seed one provisional family target under existing resource/trust/gauge policy. Its explicit seed cohort can continue improving that source-limited target. Additional experimental bodies consume pinned references and remain outside qualified shared fitting until admission. Isolation precedes graph construction, weighting and contributor-noise estimation; it also covers gauges, warm starts and reputation state ([05 §8](05-trust-and-integrity.md#8-release-gates-and-rollback)).

Keep preparation, numerical validity, empirical qualification, publication and rooted trust independent. Versioned scoped reports and queryable dependencies drive automatic assessment, promotion, invalidation and wanted shots through the existing SQLite jobs and publication-generation mechanism. Append-only arrivals queue successors without starving valid snapshots; invalidated inputs and concurrent selected-predecessor changes fence selection. Do not add another scheduler or per-camera qualification code.

The reference-body working baseline does not make other cameras ineligible for safe experimental use. Qualified multi-body activation retains the common engine and cross-body evidence gates ([09](09-roadmap.md)); missing observations are not waived. The [implementation plan](model-qualification-plan.md) records source-inspection gaps and acceptance cases, not proof that runtime qualification has passed.

## Cargo workspace

```
crates/
  looklab-color     # color science: spaces, CATs, CAM16, CAM16-UCS, ΔE, TRCs, chart data
  looklab-raw       # RAW decode wrapper (LGPL deps isolated here)
  looklab-meta      # EXIF/makernote parse, film-sim extraction, sanitization
  looklab-samples   # sample container format (versioned)
  looklab-upload    # canonical upload envelope, batch manifest, WebAuthn checks, Argon2id PoW
  looklab-prep      # align, downsample, mask, density sampling, chart/pattern detection
  looklab-prep-module # reproducible prep wasm module: plain C ABI, no imports
  looklab-model     # Look representation, evaluation, look file format, probe sets
  looklab-body      # input-transform priors, CCT estimation, chart anchoring
  looklab-fit       # single/hierarchical/joint fits, uncertainty, outlier scoring
  looklab-match     # control schemas, control-response models, optimizer, comparison reports
  looklab-export    # interface profiles, LUT baking, writers, card format (writer, reader, evaluator)
  looklab-trust     # weights, tiers, reputation, caps, gates, rollback, red-team harness
  looklab-schema    # validators for data/ (bodies, interfaces, probes); also built to WASM
  looklab-report    # static HTML/SVG reports (CLI and site)
  looklab-publish   # published static data: version manifests, artifact hashes, page data
  looklab-wasm      # wasm-bindgen facade for the browser
  looklab-web       # frontend (Leptos)
  looklab-server    # axum API
  looklab-worker    # job runner + static publisher
  looklab-cli       # native CLI
data/
  bodies/           # body definitions + control schemas (snapshots exported from the server authority)
  interfaces/       # software interface profiles
  card/             # card-format conformance vectors (docs/04 §8.6)
  probes/           # probe set definitions (versioned)
  charts/           # chart reference values (with source/licensing notes)
tools/golden/       # dev-only Python scripts producing golden test vectors from colour-science
fixtures/           # manifest + fetched/LFS/plain-git fixture policy (see AGENTS.md)
xtask/              # cargo xtask: float-method allow-list, SPDX headers, wasm32 core build, native/wasm bit identity
fuzz/               # cargo-fuzz project (its own workspace, excluded from the crate workspace); runs the sample, patch-table, upload-envelope and RAW readers
```

**Dependency DAG:**
- `color` ← `model`
- `body` ← {`color`, `raw`, `samples`, `model`}
- `samples` (independent of `model`)
- `upload` ← {`meta`, `samples`}
- `fit` ← {`model`, `samples`}
- `match` ← {`fit`, `model`}
- `trust` ← {`fit`, `model`, `samples`}
- `export` ← `model`
- `publish` ← {`model`, `schema`, `color`}
- `raw`, `meta` → `prep` → `samples`
- `schema` is a leaf utility.

Nothing in the core depends on `server`/`web`. Core crates (everything except `server`, `worker`, `cli`, `web`), **including `raw`**, must build for `wasm32-unknown-unknown` on stable Rust.

## Technology choices (candidates, not mandates)

| Concern | Candidate | Notes |
|---|---|---|
| Transcendental math | `libm` | Required in core crates for native/WASM bit-identity ([02 §8](02-color-pipeline.md)) |
| Small linear algebra | `nalgebra` | 3×3 and fixed-size math |
| Large/sparse solves | `faer` | Fitting only. **Not** in the Look evaluation path (runtime-dispatched SIMD changes summation order) |
| Optimization | hand-rolled LM on `faer` | Robust losses, warm starts |
| RAW decode | `rawler` (dnglab) | LGPL-2.1, isolated in `looklab-raw`. wasm32 needs the `uuid` `js` feature and getrandom's `wasm_js` backend; the reduced decode path spawns no rayon threads |
| JPEG decode | `zune-jpeg` | Must expose ICC/EXIF |
| HEIF (later) | TBD | Keep the decode trait ready |
| EXIF | `kamadak-exif` + custom makernote parsers | Data-driven per make |
| Serialization | `serde`, `postcard`, `serde_yaml` | |
| Compression | `zstd` | Bounded decompression ([03 §2.2](03-data-model.md)) |
| Backend | `axum`, `sqlx` (SQLite), `tower` | Single server; SQLite in WAL mode; Litestream replicates to object storage |
| Credentials | `webauthn-rs` (server); browser WebAuthn API | Passkeys only; no passwords, no email |
| Proof-of-work | memory-hard puzzle (Argon2id-based), bound to the batch manifest hash, cost ∝ images | Anti-flood only |
| Jobs | `jobs` table in SQLite, polled by the worker | Keep it boring |
| Frontend | `leptos` | `dioxus` acceptable |
| Plots | SVG from `looklab-report`; 3D cloud via WebGL/`wgpu` | |

**WASM threading:** shared-memory WASM threads need nightly Rust plus `build-std`, so v1 uses **N single-threaded workers**. Each worker processes one file at a time. No COOP/COEP headers are needed.

**Memory budget:** each worker stays ≤ 1 GB *(tentative)*: half-size CFA decode, bounded alignment grids and downsampling after the fitted radial gain. The full reduced RAW stays live until gain correction; averaging before that correction changes sample bytes. The module borrows RAW and JPEG bytes from its ABI request, and releases rawler's source copy before CFA reduction. Alignment reads the area-averaged scoring JPEG lazily rather than allocating another reduced RGB image. The RAW is decoded and aligned once per group; group members are decoded one at a time and sampled through oriented RGB8 codes, so one member's pixels are live. N is sized from a **total** budget, not the thread count:
```
total_budget = navigator.deviceMemory ? min(2 GB, deviceMemory / 4) : 1 GB
N = clamp(floor(total_budget / 1 GB), 1, 2)
```
`navigator.deviceMemory` never reports more than 8 GB, so **N ≤ 2**. It's missing in some browsers, so the fallback is conservative (N = 1). Users can raise N in settings to **at most 4** (and at most `hardwareConcurrency − 1`), with a warning about memory.

**Worker recycling:** WASM linear memory never shrinks, so freeing buffers doesn't return memory to the browser. **Before starting each file**, terminate and respawn the worker if `memory.buffer.byteLength` has passed ~75% of its budget, or after every K = 20 files *(tentative)*, whichever comes first. Checking only after the budget is crossed would let a file run over it. See [09 P5.1](09-roadmap.md).

## Data flow (contribution)

1. User drops files. Client pairs RAW↔JPEG and preselects images by expected coverage gain ([07 §1](07-contribution.md)).
2. `meta` parses and sanitizes. `raw` decodes. `prep` aligns, masks and samples. Re-render groups share one sample set.
3. The payload inspector shows exactly what will upload. The user confirms with their passkey; the batch manifest hash is signed through a WebAuthn challenge.
4. The server verifies the signature and proof-of-work, validates the schema, HMACs the serial, stores the blob, and enqueues consistency scoring.
5. The worker resolves authoritative subject/resource/evidence snapshots, applies eligibility before graphs and weights, then fits canonical targets from their explicit cohorts and camera realizations against pinned targets. Native and recipe stages remain separate.
6. Independent assessment produces immutable scoped reports and missing-evidence requests. The worker runs numerical, uncertainty and release gates, then commits eligible artifacts/report selections with lease, invalidating-dependency and selected-predecessor checks ([05 §8](05-trust-and-integrity.md#8-release-gates-and-rollback)).
7. The static publication root binds artifacts, report associations and current applicability notices. Changes publish even without new Look bytes; events arriving during a run retain successor work. Clients preserve scope and experimental status through compare, match and downloads.

## Licensing

- **Code:** AGPL-3.0-or-later, with an SPDX header in every source file.
- **Compatible inputs:**
  - colour-science: BSD-3. Port with attribution in `NOTICE` and in module docs.
  - rawler: LGPL-2.1. Dependency only, isolated in `looklab-raw`.
  - LibRaw: LGPL-2.1/CDDL. Optional FFI in the CLI only.
  - darktable / RawSpeed: GPL-3 / LGPL. Compatible with AGPLv3 via GPLv3 §13.
- **Not usable as code:** dtsolve (no license; ideas only), darktable-lut-generator (license unverified; reimplement from the method).
- **Data:** fitted targets and community data (body definitions, interface profiles) under the data license ([10 Q2](10-open-questions.md)). **Per-image samples aren't published** ([06](06-privacy-and-payload.md)).
- **LUT-defined looks** need a license declaration and attribution from the submitter ([08 §4](08-ui.md)).
- **Outputs:** generated LUTs, settings cards and exports belong to the user, and AGPL doesn't extend to them. Say this explicitly in the README and ToS. Legal review is required before launch.
