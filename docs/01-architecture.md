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
fixtures/           # manifest + small committed subset (see AGENTS.md)
xtask/              # cargo xtask: float-method allow-list, SPDX headers, wasm32 core build, native/wasm bit identity
fuzz/               # cargo-fuzz project (its own workspace, excluded from the crate workspace); runs the sample reader
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

**Memory budget:** each worker stays ≤ 1 GB *(tentative)*: half-size demosaic, downsampling immediately after decode. N is sized from a **total** budget, not the thread count:
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
5. The worker refits affected bodies and Looks in batches, applies weights and caps, runs the gates, and writes release manifests to the audit log. The static site republishes.

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
