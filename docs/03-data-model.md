# 03 — Data model

## 1. Entities

```
Body ─┬─ FirmwareVersion ──(epoch map, versioned)── Epoch ─┬─ ControlSchema
      │                                                    ├─ NativeSim ──(realizes)── Look
      │                                                    └─ ControlResponseModel (per base sim)
      ├─ InputTransform (versioned, dual-illuminant)
      └─ BodyRealizationParams (ξ, ω, clip k; versioned)

Look ── LookVersion (status, artifact, metrics, provenance, gate results)

Credential (tier, reputation, roles) ── Contribution ── ContributionImage (samples blob, sanitized metadata)
     ├─ Grant (seeder/moderator/admin, by admins)   ├─ Endorsement (by seeders)
     ├─ RootAdmission (by seeders)
     └─ Proposal ── ModerationDecision

Recipe ── RecipeScore      Bounty      AuditLogEntry (append-only)      Job
CommunityDataSnapshot (bodies/, interfaces/ exports)
```

There are **no user accounts**. Identity is a passkey credential ([05 §2](05-trust-and-integrity.md)).

### Body
`id`, `make`, `model`, `exif_aliases[]`, `kind` (`camera` | `software`), `capabilities` (`in_camera_raw_conversion`, `jpeg_color_spaces`, `heif`), `status` (`proposed` | `active`), `mapping_status` (`provisional` | `fixture_backed`).

### FirmwareVersion, Epoch, epoch map
- `FirmwareVersion`: `body_id`, `version_string`, `ordinal`, `release_date?`.
- `Epoch`: `body_id`, `ordinal`, `control_schema_version`, `native_sims[]`, `notes`, `split_from?`.
- **Epoch map:** a versioned mapping from firmware version to epoch. Contributions store `firmware_version` only. **The epoch is resolved at fit time** against the current epoch map, so splits never leave stale ids.
- Fuzzy rules:
  1. A known version maps to its epoch.
  2. An unknown version above the newest maps to the latest epoch, marked `provisional`.
  3. An unknown version between known ones maps to the nearest lower version's epoch.
  4. Splits are proposed (automatically, [02 §4.5](02-color-pipeline.md), or manually) and approved through moderation.

### ControlSchema
YAML, versioned per epoch. The server store is the authority; `data/bodies/` holds exported snapshots. A body's control schema is `data/bodies/<slug>.yaml`; its Body entity is `data/bodies/<slug>.body.yaml`. An approved mapping proposal inserts or replaces its `native_sims` entry in the epoch it names, and the updated schema is published in the next snapshot. Example (the makernote tags and values are **placeholders** that must be verified against fixtures):

```yaml
body: fujifilm/x-t5
epochs:
  - id: 1
    firmware: { from: "1.00", to: "1.99" }
    native_sims:
      - key: classic_chrome
        look: classic-chrome
        makernote: { tag: FilmMode, value: 0x0600 }     # placeholder
      - key: provia
        look: provia
        makernote: { tag: FilmMode, value: 0x0000 }     # placeholder
    exposure_modes:                                      # keys for body exposure offsets ξ
      - key: dr100
      - key: dr200
      - key: dr400
    controls:
      - key: dynamic_range
        type: enum
        values: [dr100, dr200, dr400]
        default: dr100
        look_affecting: true
        affects: [tone, exposure]
        constraints: { min_iso: { dr200: 320, dr400: 640 } }   # placeholder
        makernote:
          tag: DevelopmentDynamicRange
          values: { dr100: 100, dr200: 200, dr400: 400 }
          fallback:
            tag: DynamicRangeSetting
            values: { dr100: 0x100, dr200: 0x200, dr400: 0x201 }
      - key: highlight_tone
        type: decimal
        range: [-2.0, 4.0]
        step: 0.5
        default: 0.0
        look_affecting: true
        affects: [tone]
        makernote: { tag: HighlightTone, encoding: { scale: -16 } }
      - key: shadow_tone
        type: decimal
        range: [-2.0, 4.0]
        step: 0.5
        default: 0.0
        look_affecting: true
        affects: [tone]
        makernote: { tag: ShadowTone, encoding: { scale: -16 } }
      - key: color
        type: int
        range: [-4, 4]
        step: 1
        default: 0
        look_affecting: true
        affects: [chroma]
      - key: color_chrome_effect
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: true
        affects: [chroma, lightness]
        makernote: { tag: ColorChromeEffect, values: { off: 0, weak: 32, strong: 64 } }
      - key: color_chrome_fx_blue
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: true
        affects: [chroma, lightness]
      - key: wb_shift_r
        type: int
        range: [-9, 9]
        step: 1
        default: 0
        look_affecting: true
        affects: [wb]
        makernote: { tag: WhiteBalanceFineTune, component: 0, encoding: { scale: 20 } }
      - key: wb_shift_b
        type: int
        range: [-9, 9]
        step: 1
        default: 0
        look_affecting: true
        affects: [wb]
        makernote: { tag: WhiteBalanceFineTune, component: 1, encoding: { scale: 20 } }
      - key: exposure_compensation
        type: decimal
        range: [-3.0, 3.0]
        step: 1/3                    # stored exactly as integer thirds (ec_thirds ∈ [-9, 9])
        default: 0.0
        look_affecting: false        # shot-time setting; optimizer may recommend it
        affects: [exposure]
        makernote: { tag: ExposureCompensation, encoding: { scale: 3 } }
      - key: grain
        type: enum
        values: [off, weak, strong]
        default: off
        look_affecting: false
        spatial: true
        tolerated: [off]
        makernote: { tag: GrainEffectRoughness, values: { off: 0, weak: [16, 32], strong: [48, 64] } }
      - key: clarity
        type: int
        range: [-5, 5]
        step: 1
        default: 0
        look_affecting: false
        spatial: true
        tolerated: [0]
      - key: noise_reduction
        type: int
        range: [-4, 4]
        step: 1
        default: 0
        look_affecting: false
        spatial: true
        tolerated: any                # masked/averaged away
    solve_plan:
      - stage: tone
        controls: [dynamic_range, highlight_tone, shadow_tone, exposure_compensation]
      - stage: color
        controls: [color, color_chrome_effect, color_chrome_fx_blue, wb_shift_r, wb_shift_b]
```

**Rules:**
- `type ∈ {int, decimal, enum, bool}`. `decimal` requires `step`, given as an exact rational (`0.5`, `1/3`). Values are stored as integer multiples of the step, so ranges tile exactly.
- `makernote` (optional) names a `looklab-meta` FujiFilm tag, or the EXIF `ExposureCompensation` field, and how its raw value encodes the control: `encoding: { scale }` with `value = raw / scale` (exact rationals, integer raws on every step), or `values` mapping each declared value to the raw value(s) that mean it; `component: 0|1` indexes a paired tag such as `WhiteBalanceFineTune`, and `fallback` names the tag tried when the first is absent. Validation rejects unknown tags, incomplete enum coverage, duplicate raw values, keys off the step grid, and a scale that does not land on whole raws. A control without a mapping is not derived from a file, so `verify-payload` fails closed ([06 §5](06-privacy-and-payload.md)).
- `affects` vocabulary, and what each value moves:

  | `affects` | Moves |
  |---|---|
  | `tone` | T (and δ on T) |
  | `tint` | N |
  | `chroma` | G |
  | `hue` | R |
  | `lightness` | Λ |
  | `wb` | effective WB gain before the Look |
  | `exposure` | effective exposure (ξ/mode or EC) |

- `solve_plan` may only name declared controls.
- A `native_sims` entry sets `monochrome: true` for Acros, Monochrome, Sepia and their filter variants. Schema and per-render metadata determine classification, never equality of RGB channels. An ambiguous match set containing a monochrome sim is treated as monochrome. Complete monochrome measurements enter an explicitly authorized deferred pool; the colour fitter still refuses them ([10 Q26](10-open-questions.md)).
- Acros and Monochrome are separate base families. Filter variants retain their original native sim key and normalize to that family plus `mono_filter = none | yellow | red | green`, scoped to the body, epoch and family. WC/MG toning controls are separate; Sepia is a toned rendering, not a filter value. Applicable controls require reproduced values or explicit declarations; missing is not default.
- `look_affecting: true` controls at `default` define the **reference setting** ([02 §4.2](02-color-pipeline.md)).
- `spatial: true` controls must be in `tolerated` for an image to be used in a colour fit. Grain-on monochrome renders may enter the deferred pool with their grain settings retained; coordinate-free samples do not characterize spatial grain.
- Validated by `looklab-schema` (native in CI, WASM in the browser form).

### Documentation-derived camera capabilities

`data/manuals/` holds a separately validated `ManualCatalog`. Each camera record names its technical-documentation sources, firmware scope, documented capabilities and control domains. Documentation-derived availability is human-unconfirmed.

- This catalog is guidance, not the server-authoritative `ControlSchema`. A confirmation changes neither upload eligibility nor fixture-backed makernote mappings; those require the proposal and approval flow.
- `ManualConfirmation` version 1 carries the camera id and definition digest, reported firmware, complete or selected-control scope, and up to 64 observations. Each observation binds a control definition digest and either matches the documented options or supplies corrected availability and a typed domain.
- Agreement uses the latest claim per independent contributor identity group, control definition and reported firmware. Three eligible groups agreeing on the same actual options confirm that firmware's control. Conflicting claims require review; different firmware versions are evaluated separately.
- A camera-level confirmation requires confirmed controls with a common explicit firmware scope. Unspecified or range-scoped moderator evidence is displayed with its scope, not as an all-firmware confirmation.
- Audited moderator decisions bind the current control definition and physical or video evidence. Confirmations grant no contributor tier, reputation, calibration support or scientific weight.
- Withdrawal excludes the owner's claims from agreement and requests publication. Existing sends are fenced against restoring those camera claims; a new approval can submit a new claim. Signed records and audit history are retained.

### Look and LookVersion
- `Look`: `id`, `slug`, `name`, `kind` (`native_family` | `lut_defined`), `wb_policy`, `description`, `aliases[]`, `license` (for `lut_defined`: the submitter's declaration + attribution).
- A `lut_defined` Look's identity is the LUT bytes, its declared encoding and its base. It also stores the grid, license and attribution, and a layered approximation fitted to the composition (§3, [02 §3.8](02-color-pipeline.md)).
- `encoding` is `scene_referred { transfer, gamut }` for a LUT that consumes scene-referred log (v1: `transfer = flog2`, `gamut = fgamut`), or `display_referred { base, status }` for a LUT that expects an already-rendered image. `output` is `srgb` in v1.
- A display-referred `base` is `look@version` (a published Look; the neutral baseline `looklab-neutral-srgb-v1@1.0.0` is itself a fixed Look), `body_realization { body, renderer, film_simulation, dynamic_range, notes }` for an in-camera or software rendering, or `family_rendering { maker, rendering, dynamic_range, notes }` for an intended family Look that is not published yet. A family base resolves to a `look@version` pin when that family Look is published; until then it is `base_identified_unmeasured`. A family declaration that names a setting the native release does not carry, such as `dynamic_range`, stays `base_identified_unmeasured` rather than pinning a settings-losing base.
- `status` is `pinned` (published and fitted), `base_identified_unmeasured` (fully declared and consistent, not yet measured or published), `base_incomplete { missing }` (a declared field is absent), or `base_inconsistent { field }` (the declared setting does not exist on the declared body; `field` names it). Only `pinned` is matchable and gets a card file; the other three are stored and listed with an approximate preview at most.
- `LookVersion`: `version`, `status` (`provisional` | `rooted` | `published` | `deprecated`), `artifact_hash`, `probe_predictions_hash`, `metrics`, `provenance` (§3), `gate_results`, `changelog`. Provenance records the pinned base; a new base version creates a candidate rebased version gated against the Look's anchor, and a base revert marks every pinned dependent ([05 §8](05-trust-and-integrity.md)). Each published `lut_defined` version stores its declared base in `lut_base_pins`; a pin that names its own Look or closes a Look dependency cycle is rejected. Publishing a newer version of a base Look, resolving a pending family base to a native `look@version`, or revoking a root admission the base used writes a `lut_rebase_pending` row, and the worker consumes those rows in a `lut_rebase` job independently of image pools. A candidate publishes only after its gate passes; a held candidate leaves the previous version's bytes and matchability unchanged, and a pin is never re-anchored automatically.
- Same name ≠ same Look. Cross-body disagreement can propose a split.
- Rendering classification is orthogonal to `Look.kind`: a native-family or LUT-defined Look may be monochrome. A future monochrome response retains the common J'a'b' interface, with lightness and potentially tone-dependent tint in both chroma channels. Its model is undecided; a camera-RGB mixer followed by a tone curve is only a candidate.

### InputTransform
`body_id`, `version`, `A0_A`, `A0_D65`, `A0_source` (`ssf` | `dng` | `db` | `chart`), `delta`, `covariance` (6×6), `anchor_count`, `created_at`.

### BodyRealizationParams
`body_id`, `version`, `xi` (per exposure mode), `omega` (per WB mode class), `clip_k` (per sim), with covariances.

### ControlResponseModel
`body_id`, `epoch`, `base_sim`, `version`, `D` coefficients, `support` (controls/ranges with data), `metrics`.

`D` is stored per control and non-default value as a delta over the layers named in `affects`. Tone and tint deltas are coefficient offsets on their spline coefficients. Chroma, hue and lightness deltas use a ten-term unit-norm expansion over the layer's plane and `C > 0` volume: the `C = 0` plane, a uniform shift over the `C > 0` volume, hue cosine and sine, an `s`-linear term, a `C`-linear term, and four Gaussian hue bumps centered at 45°, 135°, 225° and 315° (σ = 40°). The bumps let a control act on a localized hue region, such as blue-only chroma. The near-neutral core stays with the base Look; control deltas do not move it. A `looklab-crm/1` file is refused: its layer coefficient vectors predate the core. Numeric controls interpolate piecewise-linearly between stored steps, with the default fixed at zero; enums and booleans select one stored value exactly.

WB controls store a 2-DOF camera-space log gain (R/B vs G) per value. Exposure stores `ξ` per exposure mode selected by an exposure-affecting enum value, and exact EV per EC step. Interactions are an optional sparse list of `(control, value)` pairs with an extra delta, empty in v1.

`support` records the values each control was observed at, including the default. A setting outside its observed range keeps its prediction and gets a variance multiplier that grows with the step distance.

The model serializes as `crm.json`:

```jsonc
{
  "format": "looklab-crm/2",
  "model": {
    "body": "fujifilm/x-t5", "epoch": 1, "base_sim": "classic_chrome", "version": 3,
    "controls": [
      { "key": "color", "default": {"step": 0}, "affects": ["chroma"],
        "values": [{ "value": {"step": 2},
                     "delta": { "layers": [{ "layer": "chroma",
                                             "entries": [[0, 0.02], [1, -0.11]] }] } }] }
    ],
    "wb": { "controls": [], "omega": [0.0, 0.0] },
    "exposure": { "modes": [], "steps": [] },
    "support": { "controls": [] }
  }
}
```

### Credential
`id` (random internal id), `webauthn_credential` (credential id + public key, as `webauthn-rs` stores it), `display_handle?` (optional, user-chosen), `tier` (`new` | `endorsed` | `seeder`), `roles` (`moderator`, `admin`), `reputation`, `created_at`, `status` (`active` | `revoked`). Status tokens are hashed rows in `status_tokens`, one per signed-in browser (capability-token status feed; [06 §4](06-privacy-and-payload.md)). Public keys and serial HMACs are never published.

### Grant, Endorsement, RootAdmission
- `Grant`: `id`, `subject_credential`, `role_or_tier` (`seeder` | `moderator` | `admin`), `scope?: { rules: [{ body, look }] }` where either side may be `*`, `granted_by` (an admin credential or `cli`), `reason`, `granted_at`, `revoked_at?`.
- `Endorsement`: `id`, `issuer_credential` (a seeder), `subject_credential`, `scope: { rules: [{ body, look }] }`, `reason`, `evidence_refs[]` (e.g. a reviewed chart session), `issued_at`, `revoked_at?`.
- `RootAdmission`: `id`, `admitted_by` (a seeder), `contribution_id`, `scope: { rules: [{ body, look }] }`, `reason`, `admitted_at`, `revoked_at?`.
- Every create/revoke writes an audit-log entry. The lists are public by internal id or display handle.

### Contribution and ContributionImage
- `Contribution`: `id`, `credential_id`, `batch_manifest_hash`, `signature`, `pow`, `kind` (client-claimed: `library` | `rerender` | `pattern` | `gradient` | `chart`), `serial_hmac?` (server-computed; absent if the contributor opted out), (the effective contributor is **resolved at fit time** from current serial links, like epochs, and never stored on the contribution; [05 §2](05-trust-and-integrity.md)), `intent` (`native` | `recipe` | `card`), `target_look_id?`, `card?` (for intent `card`: firmware id and version, look version, content-id prefix, and whether the tag was present or user-declared; [04 §8.4](04-matching-and-export.md)), `body_id`, `firmware_version`, `sim_key`, `settings`, `status` (`pending` | `accepted` | `quarantined` | `withdrawn`), `created_at`.
- `ContributionImage`: `id`, `contribution_id`, `samples_hash`, `n_samples`, `meta` (sanitized), `rerender_group?` (client-assigned id, see §2.1), `setting_source` (per control: `reproduced` from the makernote through the named snapshot's schema, or `declared` by the contributor; [06 §1](06-privacy-and-payload.md), [07 §4](07-contribution.md)), `duplicate_of?`, `fit_stats` (`rms_de`, `z`, `reason?`), `weight_components`.
- Contributions made through signed sends store the send nonce. Status groups these records as one submission; records without a send nonce are grouped by UTC day and camera as earlier activity. Status responses contain `activities[]` with nested `contributions[]`. RSS identities are stable when a submission's members change and are scoped to the credential.
- Every contribution signs `render_class` (`colour` | `monochrome`) independently of `kind` and `intent`. Deferred monochrome also signs its explicit model-development authorization. A monochrome card render has intent `card`.
- Deferred pool records retain rendering firmware, schema snapshot, original native sim, normalized family and settings with their sources, authorization time and a fixed 180-day expiry. Their scientific weight is zero until monochrome analysis exists; they never enter colour fit, calibration, gauge, support, release scores or reputation.

### Proposal and ModerationDecision
- `Proposal`: `id`, `credential_id`, `type` (`body` | `schema_change` | `mapping` | `interface_profile` | `lut_look` | `epoch_split` | `look_split` | `gate_override`), `payload` (validated YAML/JSON), `human_summary` (a menu-level description, e.g. "adds Reala Ace to X-T5 firmware 2.0+"), `evidence[]` (confirmations carrying parsed whitelisted tag values; fixture donations; worker evidence), `status`, `created_at`.
- `ModerationDecision`: `proposal_id`, `moderator_credential`, `decision`, `reason`, `decided_at`. Logged.
- **Moderation actions** beyond proposals (also logged): re-anchor a Look, revert to version N excluding credential(s) K ([05 §8](05-trust-and-integrity.md)), handle serial-reuse and new-credential-burst flags, review endorsement nominations.

### Recipe, RecipeScore, Bounty
- `Recipe`: `id`, `credential_id?`, `body_id`, `epoch`, `base_sim`, `settings`, `target_look_id`, `source`.
- `RecipeScore`: `predicted_de`, `measured_de`, `n_verifications`, model versions used.
- `Bounty`: `look_version_id`, `body_id?`, `region`, `expected_variance_reduction`, `reason`, `status`.

### AuditLogEntry, Job, CommunityDataSnapshot
- `AuditLogEntry`: `seq` (autoincrement), `type`, `payload` (JSON text), `ts`. Append-only (no UPDATE/DELETE in application code). Published as a page and JSON ([05 §11](05-trust-and-integrity.md)).
- `Job`: `id`, `kind`, `payload`, `status`, `attempts`, `run_after`, `locked_until`. The worker polls this table.
- `CommunityDataSnapshot`: `version`, `content_hash`, export of approved bodies/schemas/interfaces plus the fixture consent list.
  A snapshot lives in `data/community/v<version>/` with `bodies/`, `interfaces/`, `fixtures/manifest.toml`, published fixture bytes under `fixtures/` or `fixtures/donated/` and `snapshot.json` (`{version, content_hash}`);
  the hash covers each file's relative path and bytes in path order, and `looklab validate-snapshot <dir>` checks the files, the hash, each document's id against its path, each manifest entry's sha256, each published fixture's bytes at the path its `committed` value names, and that every `fixture_backed` body has a manifest entry.
- An approval records the version it publishes before the directory rename; a failed rename leaves a `pending` publication that the server retries rather than an approval without its export.

## 2. Samples: preparation and format

### 2.1 Preparation (client, `looklab-prep`)
1. **Decode.** RAW → linear camera RGB (half-size or bilinear demosaic). JPEG → code values + color space.
2. **Geometry and shading.** Apply orientation. Use embedded lens-correction parameters from the makernotes where present. Otherwise fit during alignment:
   - distortion `(k1, k2)`;
   - **radial gain** `g(r) = 1 + a1 r² + a2 r⁴ + a3 r⁶` applied to RAW luminance, which models in-camera vignetting/shading correction.

   Alignment fits a temporary bounded nonnegative RGB intensity projection and offset for each rendering. The radial gain and a temporary 12-knot monotone tone map are fitted on that intensity; geometry refinement and quality statistics use the same projection. Only geometry and gain are kept. The sample measurements retain all three RGB channels.
3. **Align.** Similarity + radial distortion via multi-scale phase correlation, then LM refinement on normalized cross-correlation. Reject if central correlation of the projected intensity is < 0.98.
4. **Downsample** to ~1.5 MP (area average).
5. **Mask:** clipped RAW channels (≥ 0.98 of white), local gradient above the 90th percentile, 3% border, chart/pattern regions (handled separately).
6. **Density sampling** in `(s, h_in, √C_in)` using the body's prior transform at the image's CCT, where `s = log2(Y/0.18)` clamped to [−12, +6] and `(h_in, √C_in)` come from CAM16-UCS of the chromaticity scaled to Y = 0.18 ([02 §3](02-color-pipeline.md)). Up to N = 50,000 samples, inclusion probability ∝ 1/bin_count (capped). Store `density_w`, the inverse inclusion probability of the actual selection (1.0 when every candidate is selected); the fit consumes it as a sample weight.
7. **Re-render groups:** for JPEGs rendered in camera from the same RAW, sample **one shared pixel set** and apply **one shared shuffle**. The members' `in_rgb` columns are then identical. That gives the control-response fit exactly matched inputs, and lets a future server-side check confirm groups without any RAW hash.
8. **Shuffle.** No spatial information leaves.

**Sampling seed (privacy invariant).** Density sampling and the shuffle use a **fresh CSPRNG seed per image (or per re-render group)**. It's never derived from anything the server knows (batch nonce, ids, hashes), never uploaded and never stored server-side. Anyone who knew the seed could undo the shuffle and recover sampling order, which could be close to raster order. The client can export the seeds **locally** as a seed file, so users can reproduce the payload byte for byte ([06 §5](06-privacy-and-payload.md)). Seeds are **sensitive**: anyone holding a seed and the upload can recover the sampling order. The UI labels them as such and warns against pasting them into bug reports. Fixed seeds are allowed only in tests.

**Reproducibility.** The payload depends on more than color math: RAW/JPEG decode, FFT and LM alignment, the gain fit, CCT estimation and downsampling. Native builds pick SIMD paths at runtime, so a native CLI can't reproduce the browser's bytes. The reference prep is therefore the **`looklab-wasm` prep module**. The CLI's `verify-payload` command runs that module, built by the user from source, under wasmtime. Identical inputs, seed file and prior-transform version give identical bytes by construction.

Monochrome preparation keeps all three JPEG output channels, the declared output colour space, density weights, flags and shared re-render grouping. Mixed colour/Acros/Sepia groups reuse one RAW decode, input sample set and shuffle. Prep version 2 samples each JPEG through its own validated alignment at the shared RAW-selected points; version 1 reused the reference JPEG's mapping for every member. The sample container remains version 1. No source image or RAW hash is added to the payload.

### 2.2 Sample blob format (version 1)
Little-endian. The body is zstd-compressed.

```
magic        "LLSAMP"   6 bytes
version      u16 = 1
header_len   u32 (≤ 4096)
header       postcard(SampleHeader)
body         zstd(columns)        // decompression limited to n × ROW_BYTES (29 bytes/row) + 1 KiB

SampleHeader {
  n: u32,                         // 1 ≤ n ≤ 65_536
  input_space: InputSpace,        // enum: CameraRgbWbLinear
  output_space: OutputSpace,      // enum: Srgb | AdobeRgb
  wb_multipliers: [f32; 3],
  cct_hint_k: f32,                // server recomputes
  exposure_hint_ev: f32,          // absolute EV at ISO 100; not the fit's x_meta (02 §2)
  rerender_group: Option<[u8;16]>,
  prep_version: u32,              // registered prep version id
  prior_transform_version: u32,   // body input-transform version used for density sampling
  prep_params_hash: [u8; 32],
}
columns:
  in_rgb    f32 × 3 × n
  out_rgb   f32 × 3 × n
  density_w f32 × n
  flags     u8 × n     // bit0 chart, bit1 pattern, bit2 near-clip output
```

`exposure_hint_ev` is the capture's absolute EV at ISO 100, from the ISO 2720:1974 exposure equation on ISO, shutter and aperture. It is reported, not consumed: the fit's per-image exposure prior mean `x_meta` is a relative offset against the body's exposure gauge ([02 §2](02-color-pipeline.md)), which an absolute EV is not, and `in_rgb` is already image-referred. Prep writes it; the CLI `fit` and the worker's `weights` and `refit` set `x_meta = 0` rather than converting it.

The reader rejects: unknown enum values, n out of range, a header over the limit, decompressed size ≠ expected, NaN/Inf, and out-of-range values. Chart and pattern patch tables are separate small blobs with the same limits.

**Value limits (version 1):** `in_rgb`/`out_rgb` and patch-table means finite in [-0.5, 64.0]; `density_w` finite in (0, 10⁶]; patch-table standard deviations finite in [0, 64]; `wb_multipliers` finite in [0.01, 64]; `cct_hint_k` in [0, 100 000] (0 = unknown); `exposure_hint_ev` within ±16; sample and patch-row counts in 1..=65 536; flags bits above bit 2 rejected; an encoded header is at most 4096 bytes; a zstd frame either declares its content size, which must equal the expected column bytes, or needs a window of at most 1 MiB, so decompression reads at most n × 29 + 1024 bytes and the codec holds at most one decoded block, bounded by the frame window. The C `zstd` crate does not build for `wasm32-unknown-unknown`, so both blobs use the pure-Rust `ruzstd` codec (`Fastest` level, single-threaded, no dictionary) in `looklab-samples`.

### 2.3 Upload envelope (canonical)
An upload is **one `application/octet-stream` body** in a single canonical encoding: no JSON, no multipart, no optional whitespace or field order. Any data a client hid in formatting would make the body non-canonical and fail verification.

```
magic            "LLUPLD"   6 bytes
version          u16 = 3
nonce            [u8; 32]                    // server-issued, one per send
send_manifest    len:u32 + postcard(SendManifest)
manifest         len:u32 + postcard(BatchManifest)
                 // fixed-schema structs only (no free-form maps): per image, its
                 // sanitized metadata, rerender_group, and blob sha256
webauthn         credential_id      len:u8  + bytes (≤ 255)
                 authenticator_data len:u16 + bytes (≤ 512)
                 client_data_json   len:u16 + bytes (≤ 1024)
                 signature          len:u16 + bytes (≤ 512)   // Ed25519, ECDSA (DER), or RSA
pow              k:u8 = 16, then k × u64 solutions
serial           tag:u8 (0 = absent, 1 = present) [+ len:u8 + ASCII bytes (≤ 64)]
blobs            count:u16, then count × (len:u32 + sample blob)   // order = manifest order
(end)            no trailing bytes
```

`SendManifest { version: 1, batches: Vec<ManifestHash>, attestations: Vec<AttestationHash> }` is the manifest one passkey approval covers. Each collection is independently sorted in strictly increasing lexicographic order; duplicates, reordered hashes and undeclared content are rejected. Defaults allow at most 64 batches and 8 attestations, with at least one item across both collections. An attestation-only send needs no photo batch.

Envelope version 4 carries this send-manifest layout. Version 3 bytes are rejected before nonce consumption with `426 Upgrade Required` and `{"error":"ENVELOPE_VERSION_UNSUPPORTED","action":"reload_and_prepare"}`; clients reload and prepare a fresh send rather than reusing the old assertion. The previous live client ignores response bodies and can only show `upload: HTTP 426`; it cannot display the reload guidance. Deployment must tell owners to reload the tab and prepare the send again. No compatibility shim or automatic retry exists.

One nonce identifies one send instance. The WebAuthn challenge is `H(nonce ‖ send manifest hash)`, signed once for the whole send; every batch and confirmation request carries the same send manifest and assertion. Proof of work stays per photo batch, bound to `H(nonce ‖ batch manifest hash)` ([05 §9](05-trust-and-integrity.md)). A confirmation may reserve the send before its photo batches arrive. The server binds the first accepted request's credential, manifest and assertion, tracks both collections, and rejects mismatched, duplicate, completed, expired or withdrawn sends. Accepted photo batches are retained if a later optional confirmation fails. The same content approved under a fresh nonce is a new send.

The server parses this strictly and rejects anything non-canonical. All of it is produced by the `looklab-wasm` module; the JavaScript layer only moves bytes ([AGENTS.md](../AGENTS.md)).

`BatchManifest` carries `community_snapshot_version`, the snapshot whose control schema its settings were read against. The server keeps every snapshot it has published and resolves the batch body and settings against that version, not the current one; an unknown version fails with `SNAPSHOT_UNKNOWN` and one older than the retained snapshots with `SNAPSHOT_TOO_OLD`. Each settings entry also carries its source, `reproduced` or `declared` ([07 §4](07-contribution.md)).

`BatchManifest` also carries the signed `render_class` and optional monochrome model-development authorization. The server checks classification against the named schema and every image's sanitized rendering metadata before accepting a batch; changing the batch's label cannot route known monochrome measurements into colour analysis. The sample-blob format remains version 1.

Manual confirmations use canonical JSON request bytes from `looklab-wasm`; whitespace, alternate escapes, field-order changes and unknown fields are rejected. Their attestation hashes bind the canonical confirmation payloads. A standalone send follows the same credential and anti-abuse rules; it grants no photo contribution or scientific credit.

## 3. Look file format

`look.json` + `look.bin`, packaged as a `.looklab` zip.

```jsonc
{
  "format": "looklab-look/2",
  "look": { "slug": "classic-chrome", "version": "3.0.0", "kind": "native_family", "wb_policy": "adaptive" },
  "rendering": { "maker": "Fujifilm", "name": "Classic Chrome" },
  "viewing_conditions": { "white": "D65", "L_A": 4.074, "Y_b": 20, "surround": "average", "discount_illuminant": true },
  "probe_set": "v1",
  "layers": {
    "tone":      { "kind": "spline", "knots": [], "coef_ref": "bin:T" },
    "tint":      { "knots": [], "coef_ref": "bin:N" },
    "chroma":    { "hue_knots": 24, "s_knots": [], "c_knots": 4, "c_max": 120, "core_terms": 5, "core_radius": 2, "coef_ref": "bin:G", "reference": "colorimetric_at_neutral_matched_luminance" },
    "hue":       { "coef_ref": "bin:R" },
    "lightness": { "coef_ref": "bin:Lambda" },
    "residual":  { "grid": [17,17,17], "domain": {}, "coef_ref": "bin:res" }
  },
  "output_clip": { "function": "tanh_soft_v1", "k": 0.03 },
  "regularization": { "lambda_T": 0, "lambda_N": 0, "lambda_GRL": 0, "lambda_res": 0 },
  "uncertainty": { "probe_cov_ref": "bin:probe_cov", "density_ref": "bin:kde", "method": "bootstrap|sandwich" },
  "provenance": {
    "bodies": [{
      "body": "fujifilm/x-t5", "epoch_map_version": 4, "epochs": [1,2],
      "input_transform_version": 7, "realization_params_version": 3,
      "control_response_version": 2, "clip_k": 0.031, "images": 412, "credentials": 9
    }],
    "root": { "admissions": ["<root admission ids>"], "anchor_version": "2.0.0" },
    "community_snapshot_version": 12,
    "prep_versions": [3],
    "created": "2026-11-03T00:00:00Z",
    "software": "looklab 0.4.0"
  }
}
```

A `lut_defined` Look stores the uploaded LUT, its declared encodings, base and license, plus a fitted layered approximation. The approximation is fitted to the composition and frozen, so evaluation, previews, cards and matching never read the base Look at runtime.

```jsonc
{
  "look": { "slug": "ricoh-griv-cinema-green-flog2", "version": "1.0.0", "kind": "lut_defined", "wb_policy": "adaptive" },
  "definition": {
    "name": "Ricoh GR IV Cinema Green (F-Log2)",
    "license": { "own_work": false, "source_license": "CC0-1.0", "attribution": "mistycycles (Discord), donated 2026-11-05" },
    "encoding": { "kind": "scene_referred", "transfer": "flog2", "gamut": "fgamut" },
    "output": "srgb",
    "lut": { "size": 33, "domain_min": [0, 0, 0], "domain_max": [1, 1, 1], "values_ref": "bin:lut" },
    "notes": []
  }
}
```

A `lut_defined` Look may declare what it reproduces: `emulates = camera_rendering { maker, rendering }` or `film_stock { name }`; absent means an original grade. A native-family Look declares the rendering it reproduces in `rendering = { maker, name }`; a camera-rendering emulation matches when both labels agree after case folding and ignoring spaces and punctuation. A LUT that reproduces a rendering a released camera already produces officially is interim. When a native-family Look for that rendering is published, the LUT Look gets `superseded_by` pointing at it (`slug@version`) and stays listed with that pointer until a moderator hides or removes it; both actions write an audit-log row. A film-stock emulation or an original grade is first-class and is never superseded by a camera rendering. A Look's listing state is `listed`, `hidden` or `removed`; hidden and removed Looks keep their versions and artifacts in the database but are left out of the published index, and their published pages and downloadable artifacts are removed from the site.

A display-referred LUT replaces `encoding` with `{ "kind": "display_referred", "base": { "kind": "body_realization", "body": "fujifilm/x-e3", "renderer": "x_raw_studio", "film_simulation": "classic_negative", "dynamic_range": "dr400", "notes": "..." }, "status": { "kind": "base_inconsistent", "field": "film_simulation" } }`. `status.kind` is `pinned`, `base_identified_unmeasured`, `base_incomplete` or `base_inconsistent`; the last three are stored and listed but not matchable and have no card file. A `base_identified_unmeasured` family base becomes `pinned` in a new version when a native Look for that rendering is published, unless the declaration names a setting the native release does not carry; the rebased approximation is refitted against that `look@version` rather than the neutral preview. Provenance repeats the pinned base as `base`; a base publish or root revocation marks the version in `lut_rebase_pending` without rewriting its immutable file ([05 §8](05-trust-and-integrity.md)).

`look.bin` is a named-array container: `LKBN`, a little-endian `u32` array count, then per array a `u8` name length, the ASCII name, a little-endian `u32` element count and that many little-endian `f64`, arrays sorted by name. v2 names are `T` (absent for a colorimetric tone), `N`, `G`, `R`, `Lambda`, `res`, and optionally `probe_cov` and `probe_cov_post` (9 `f64` per probe, row-major 3×3), `probe_sigma`, `probe_sigma_post`, `probe_bias`, `probe_pre_reliable` and `support` (one `f64` per probe), `kde` (one `f64` per probe) and `lut` (`size³ × 3` `f64`, red fastest, for a `lut_defined` Look); `res` is channel-minor, `((s·17 + a)·17 + b)·3 + channel`, and holds the 17³ cubic B-spline control points. Each `G`/`R`/`Lambda` array is 780 `f64`: 10 plane, 720 volume, then 50 near-neutral core coefficients indexed `[s][term]` for the five normalized Cartesian monomials of docs/02 §3.3. v2 fixes the knot vectors, residual domain and core join radius, repeated in `look.json` for validation; a stored LUT side is at most 64, checked before allocation; the zip is stored, fixed-order and fixed-timestamp. A `looklab-look/1` file is refused with a format error: its coefficient meaning has no exact v2 representation, so it must be refitted and republished under v2.

**Evaluator semantics (v2).** Evaluation follows [02 §3](02-color-pipeline.md): physical `s = log2(Y/0.18)` and the bounded spline coordinate `s_L = E(s)` (identity on `[−12, +6]`, C² transitions outside it with widths `S_SHADOW_TRANSITION = 2` and `S_HIGHLIGHT_TRANSITION = 1` stops, saturation at `−14` and `7`). The tone, G/R/Λ and residual use `s_L` and extrapolate their end cubic pieces outside the knot range; the colorimetric tone uses physical `s`. The tint continues C² above `J' = 100` to `J'_max = solve::neutral_j_prime_max()` and is constant above it. The near-neutral coordinates are `n = K·(a, b)/D` (pre-power cone-opponent coordinates of the input at `Y = 0.18`, `K = C0·D_ref/ρ0` from the reference viewing conditions). The core input is `n`. Two pre-clip chroma paths are evaluated: a CAM16-UCS path with the layers in UCS, and an internal path with the layers on the normalized opponent reference `N` of the reference stimulus. The published chroma is `w·UCS + (1 − w)·Φ(internal)` with `C0 = 2`, `w = 1 − (1 − |n|²/C0²)³` and `Φ` the exact CAM16-UCS map of the reconstructed internal stimulus (`Cam16Model::ucs_chroma_from_opponent`); `Φ(N_ref) = UCS`, so the identity Look is colorimetric on every probe and outside `|n| ≥ C0` the UCS path is published unchanged. The residual lookup coordinates are `w(|n|)·UCS + (1 − w(|n|))·n` and the residual is the 17³ cubic B-spline. The fitting residual is `w·(post-clip UCS − observed UCS) + (1 − w)·(post-clip internal − Ψ(observed))`, with `Ψ = Φ⁻¹` applied once per sample. The file fixes the knot vectors, residual domain, core terms and join radius; the exposure transition widths, the tint continuation end and `K` are evaluator constants derived from the tone range and the reference viewing conditions. Out-of-domain inputs are handled by the domain predicates and the interior barrier of [02 §4.1](02-color-pipeline.md), not by a clamp.

A body's control-response model is a separate `crm.json` (§1), versioned and referenced from provenance by `control_response_version`.

## 4. Storage notes

- SQLite (WAL mode) for entities, replicated continuously with Litestream. Settings/metadata are JSON text validated in the application. UUIDs are stored as text. Migrations via `sqlx migrate`.
- Object storage keyed by `sha256`. Sample blobs are **private** (never published). Artifacts are public and addressed by hash.
- **Withdrawal:** a request signed by the contributing credential. The data is excluded from the next fit. Published versions are immutable. Blobs are hard-deleted after 30 days. Erasure for users who lost their credential is pending legal review ([10 Q3](10-open-questions.md)).
- Don't store IP addresses. Rate limiting is in memory only.

## 5. Published static data

The site is a directory of immutable files plus JSON indexes, written by `looklab-publish`. The crate has no filesystem access: `write_site` takes a `SiteSink` (one `put(path, bytes)` call per file), so the same writer runs natively and in a browser test. Look pages and the index carry `matchable` per version; a non-matchable version is listed and previewed but excluded from matching controls and match requests.

```
data/index.json                          SiteIndex: bodies, looks, versions, scenes
data/audit-log.json                      AuditLog (docs/05 §11)
data/bodies/<make>/<model>.json          BodyPage
data/looks/<slug>.json                   LookPage
data/looks/<slug>/<version>/manifest.json VersionManifest
data/looks/<slug>/<version>/<artifact>   artifact bytes, for example look.looklab
data/scenes/index.json                   SceneIndex
data/scenes/<name>.png                   RGB16 PNG of shaped scene-linear Rec.2020 coordinates
data/scenes/<name>-thumb.png             thumbnail in the same coordinate format
```

**VersionManifest.** `{ look, version, artifacts: [{ name, sha256, bytes }], status, gate_summary, provenance }`. `status` is `provisional | rooted | published | deprecated`; `gate_summary` lists each release check and its result; `provenance` summarizes bodies and epochs, root admissions, the anchor version, the community snapshot, prep versions, the creation time and the producing software. Every artifact is addressed by the lowercase hex SHA-256 of its bytes. `verify_artifact(manifest, name, bytes)` returns `HashMismatch` when the name is unlisted or the size or digest differs.

**Client rule.** A page downloads an artifact only through its manifest and verifies it before parsing. A mismatch is an error and nothing is rendered from the bytes. The Look page shows each artifact's digest and a "matches version manifest" indicator after a successful check.

**Authenticated index.** `data/index.json` is the single publication root. It lists every version's `manifest_sha256` and byte count, and every scene's `sha256` and byte count. A serverless preview embeds the digest from `site/site-root.txt` in its wasm. An online build fetches the current digest from `/data/site-root.json` at its configured API origin and fetches the catalog and publication artifacts from that same authority. The client rejects an index whose SHA-256 differs, then verifies each manifest and scene against the index before parsing their contents. These checks detect corruption and partial publication. The live root endpoint is trusted authority, not protection against a malicious API; a malicious application origin can also replace the client and its embedded pin. The CLI and reproducible builds provide the independent verification described in [06 §5](06-privacy-and-payload.md).

**Body page data** is derived from the body and control-schema YAML snapshots: make/model, epochs and firmware ranges, capabilities, mapping status, the control schema per epoch with control differences, native sims, plus page-only facts the schema does not hold: input-transform confidence (source, anchor count, tier) and plain-language realization notes. The writer joins native sims to the latest published look version.

**Match bundles.** A matchable version carries one bundle per `(body, epoch)`. The descriptor `match-<body>-<epoch>-sims.json` (`looklab-match-bundle/1`) names, for each ranked sim, the base realization Look (`match-<body>-<epoch>-<sim>-base.looklab`), its control-response model (`...-<sim>-crm.json`) and optional predicted covariance (`...-<sim>-covariance.json`), and lists every native sim the bundle leaves out with `no_published_look` or `no_response`. The schema snapshot (`...-body.yaml`) and input transform (`...-transform.yaml`) are shared. Consumers verify every artifact through the version manifest before parsing it, so a bundle is only usable as a whole. The consumer reads `manifest.json` from the version directory, verifies the descriptor against it before decoding, then verifies and parses the schema snapshot, transform, base Looks, response models and covariances from the same directory. Each descriptor artifact name must be the canonical `match-<body>-<epoch>-<sim>-...` name derived from the descriptor's body, epoch and sim, so a descriptor cannot name files outside its version directory. Each response model must name the descriptor's body, epoch and sim.

The worker assembles a bundle for every active camera body epoch in the version's release provenance before writing the version manifest, from the realizations published at that moment plus the release's own in-memory candidate sim. The bundle artifacts are stored with the version and listed in its manifest; a later source-sim release, schema update or input-transform update does not change the version's bytes or manifest. A version published before bundles were bound this way has no bundle and does not gain one retroactively. A sim whose Look has no published realization on the body, or whose realization has no response model for the epoch, is listed in the descriptor with the matching reason. A hidden or removed Look contributes neither a target version nor a ranked sim. Clients verify the bundle artifacts through the manifest like any other artifact. The fit stores no control-response covariance yet, so bundles omit `covariance.json`.

**Look page data** repeats the identity (`slug`, name, kind, `wb_policy`, description, aliases, license for LUT-defined looks) and lists each version's manifest, changelog, weak spots and any publication notice (a since-revoked root admission, or a revert).

**Previews.** A version may publish `preview.lut33`, a 33³ grid as little-endian `f32` over a scene-linear log2 domain: each axis is Rec.2020 linear RGB, shaped as `0.5 + 0.5 · log2(1 + x / 0.18) / log2(1 + 2⁶)` above zero and `0.5 - 0.5 · log2(1 - x / 0.18) / log2(1 + 2¹²)` below, so HDR highlights and wide-gamut colours reach the Look instead of clipping at sRGB white. Grid nodes are decoded back to XYZ, evaluated through the Look with its output clip, and stored as encoded sRGB. The browser shapes each scene pixel the same way, applies the LUT with tetrahedral interpolation, and writes sRGB to a canvas. The LUT is an artifact like any other, so it is checked before use and a look switch costs only the LUT application.

**Card files.** Every published version includes `card.llk`, the firmware card file of [04 §8](04-matching-and-export.md). It is body-agnostic, listed in the manifest like any artifact, and checked before use under the client rule.

**Canonical scenes** originate as scene-linear D65 XYZ and are stored as RGB16 PNGs of the preview shaper's Rec.2020 coordinates, quantized to 12 bits (`looklab-scene/rec2020-shaper-png16/1`). These PNGs are LUT coordinates, not display-ready images. Full scenes have at most 1600 px on the long edge; thumbnail variants have at most 400 px. The sample site publishes six CC0 RAW photographs alongside the neutral and hue gradient, hue wheel, and skin, foliage and sky test charts.

`SceneIndex` and the authenticated `SiteIndex` list each scene's `name`, typed `kind` (`photo | test_chart`), display `title`, optional `credit` (`text`, `source_url`, `license`, `source_sha256`), dimensions, `path`, `sha256` and `bytes`. The nested `thumbnail` records its own dimensions, path, digest and byte count. Clients verify full scenes and thumbnails against the authenticated index before decoding.

**Sample data.** `cargo run -p looklab-publish --features photo-scenes --example site_data` fits three synthetic Looks, builds their manifested sample camera match bundles, then computes Release audit entries from the final manifests. It publishes bodies, photographic scenes, test charts and the audit log, writing the tree above plus `site/site-root.txt` to `site/`. `publish_match_fixture` in `looklab-wasm` adds the separate `synthetic-match` test fixture without rewriting public look manifests. `sh site/scripts/build-site.sh --release --dist <dir>` embeds the root digest and produces a site that renders with no server.

`looklab publish --db <sqlite> --blobs <dir> --data data --transforms data/transforms --out site` writes the same tree from the server database: published Look versions and their manifested artifacts, the match bundles of every published body epoch, body pages from `data/bodies/`, and the release entries of the audit log under `site/data/`, with the index digest at `site/site-root.txt`. It publishes the photo scenes prepared under `data/scenes/` (one PNG per scene plus a provenance index; see [ops/deployment](ops/deployment.md)), verifying each PNG's SHA-256 first. Without that directory it publishes the test charts only.
