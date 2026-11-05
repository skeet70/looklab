# 04 — Matching and export

Given a user's body *B* (epoch *e*) and a target Look *L*, produce one of:
- **Settings** for stock firmware: a base native sim, control values, and exposure compensation / WB shift where useful;
- **A card file** (§8) for hacked firmware. It is body-agnostic; the firmware turns it into whatever its pipeline can set; or
- **An artifact** for a software pipeline, per a declared interface profile (§6).

Probes are defined in adapted scene XYZ, so the target on any body is `L(p)`. The body enters through its realization parameters (stock path) or through the bake (artifact path).

## 1. Matching levels

| Level | User provides | Output |
|---|---|---|
| A | nothing / RAWs | Target renders of the user's RAWs (via `A_B` + L) |
| B | RAW+JPEG from their current setup | Fitted realization of the current setup + diagnostic comparison vs. target |
| C | body only (uses the crowd model); optional verification loop | Recommended settings + predicted error map + unreachable regions |
| C+ | hacked firmware that implements §8 | Card file; verification through in-camera renders (§5, §8.4) |
| C+ | software interface profile | Baked artifact + validation report |

## 2. Realization model on a body (stock firmware)

The realization of base native sim *k* on body *B* at settings *u* has two parts:

```
θ(k, u)       = θ_L(k) + δ_fw + Σ_j φ_j(u_j) · D_{B,k,j} + Σ_{(j,l)∈I} φ_j(u_j) φ_l(u_l) · D_{B,k,jl}
input effects = WB gain ω_B · W(u_wb)  and  exposure 2^(ξ_{B,m(u)} + EC(u))
```

- `θ_L(k)`: the Look family that sim *k* realizes ([02 §4.2](02-color-pipeline.md)). θ and D are fitted **jointly** from all native data on the body.
- `φ_j`: a per-control basis. Integer/decimal controls get one coefficient vector per step offset from default (piecewise-linear, so nonlinear responses can be learned). Enums get one vector per non-default value.
- `D_{B,k,j}`: restricted to the layers listed in the control's `affects` ([03 §1](03-data-model.md)). This is a strong prior that keeps the model identifiable with little data.
- `I`: sparse interactions, declared in the schema or added when residuals demand. Default: none.
- **`wb` controls** act as a 2-DOF gain on camera RGB before the Look. **`exposure` controls** (mode and EC) act through ξ and EC. Neither changes θ.
- Learned from re-render series (most informative), all native library data, and recipe uploads (recipe stage only, [02 §4.3](02-color-pipeline.md)). Uncertainty comes from the image-clustered sandwich estimator. Settings far from observed data get inflated uncertainty.

**v1 storage.** `T` and `N` deltas are coefficient offsets. `G`, `R` and `Λ` deltas are a ten-term unit-norm expansion over the layer's coefficient vector (`C = 0` plane, uniform `C > 0` volume, hue cosine and sine, `s`-linear, `C`-linear, and four Gaussian hue bumps), which keeps a control with many steps to a few coefficients per step while allowing hue-localized responses. The fit regularizes each control's expansion with shrinkage toward zero plus second differences across adjacent numeric steps; generalized cross-validation chooses the weight. Deltas at the default are exactly zero, and numeric interpolation is piecewise-linear between stored steps. `support` records the observed values; a probe or candidate beyond it keeps its prediction and has the response covariance multiplied by `1 + d`, where `d` is the step distance outside the observed range (unobserved enum values use 2). [03 §1](03-data-model.md) gives the `crm.json` layout.

## 3. Settings optimizer

Objective for a candidate (k, u):

```
E(k,u) = Σ_p w_p c_p · ΔE_UCS( L(p), Real_{k,u}(p) )²  +  β Σ_p w_p · tr Cov_pred(k,u,p)
```

- `w_p`: probe group weight times the user's emphasis (balanced / skin / landscape / product).
- `c_p = 1/(1 + σ²_L(p)/τ²)`, with τ = 1 ΔE. Unsupported probes are excluded.
- β = 0.25 penalizes extrapolation.
- Emphasis multiplies the [02 §6](02-color-pipeline.md) group weights:

| Group | balanced | skin | landscape | product |
|---|---|---|---|---|
| Gray ramp | 1.0 | 1.0 | 0.8 | 1.2 |
| Chart patches | 1.0 | 1.0 | 1.0 | 2.0 |
| Hue grid | 1.0 | 0.8 | 1.25 | 1.0 |
| Skin | 1.0 | 3.0 | 0.5 | 0.8 |
| Memory colors | 1.0 | 1.2 | 3.0 | 1.0 |

**Budget: < 2 s for a full search in one WASM worker** (p95 across fixture bodies). The same budget applies everywhere ([09 P2.3](09-roadmap.md)). To meet it:

1. **Prune base sims.** Evaluate every native sim at defaults (with the best EC and WB found by 1-D sweeps). Keep the top 3.
2. **Search on a probe subset.** Use 500 probes, stratified by group and hue, with weights rescaled. Rescore the final top 5 on the full probe set.
3. **Per-probe linearization.** For each kept base sim, precompute at its default the per-probe output `o_p` and the Jacobian `J_p` with respect to the coefficients D can move, plus the WB and exposure input effects. Then precompute every control value's contribution `c_{p,j,v} = J_p D_{B,k,j} φ_j(v)` (a 3-vector). Evaluating a candidate is then a handful of 3-vector adds per probe. Exactly re-evaluate the top candidates. If linear and exact predictions differ by > 0.3 ΔE median, re-linearize at that candidate and continue (a trust-region style step).
4. **Low-rank predicted covariance.** Rank-20 eigen-approximation of Cov(D); per-control-value trace contributions precomputed in the same additive form.

Cov(D) arrives as `looklab-crm-cov/1`: the control-response model's `parameter_ids`, row-major covariance over them (D and WB). The optimizer splits it by `(control, value)`, eigendecomposes each block, keeps the 20 largest positive eigenvalues, and applies `Support::inflation(settings)` to the resulting trace.

**Search:** for each kept base sim, run staged integer coordinate descent following the schema's `solve_plan` (tone stage, then color stage; each control swept over its range; iterate to convergence). Then 3 perturbation rounds (random joint ±1–2 step moves on 2–3 controls, accepted if better). Keep a beam of 5 per sim and return the top 3 overall. Defaults: 500-probe subset, 5-probe full rescore, 4 coordinate passes, 1 re-linearization, 2 exact warm restarts for the leading sim, seed supplied by the caller.

Large nonlinear moves, such as a two-stop tone change together with exposure compensation, can be pruned by a linearization at default. The leading sim therefore gets a coarse exact coordinate descent on 64 probes, plus two random-restart descents on 48 probes, and the beam seeds are exact-scored on the full set. On 3 synthetic sims and the 1057-probe set a clean native release run measures median 0.72 s, p95 0.77 s; the same example under wasm32-wasip1 on wasmtime measures median 1.10 s, p95 1.26 s. Runs under concurrent build load reach a native p95 near 2.0 s.

## 4. Settings card and permalinks

The **settings card** shows:
- The base sim and every control value, in the camera's menu order and wording, **including exposure compensation and WB shift** when they help (e.g. "this body renders 0.5 EV darker in DR200: set EC +0.3 and WB shift R+1").
- Predicted ΔE median / p95 with the uncertainty band.
- A per-layer residual summary in natural language.
- Unreachable regions, e.g. "Cyan chroma can't be reduced enough; the remaining error is 3.8 ΔE."
- Confidence, which is low when the control-response model is thin.

**Permalinks** pin everything needed to reproduce the result: Look version, base realization Look version, control-response model version, input transform version, realization params version, epoch map version, probe set version, optimizer version and the user's emphasis. All versions are immutable, so a permalink always reproduces.

The canonical encoding is a single line of `key=value` fields joined by `|`, starting with `looklab-match/1`, in this order: `look`, `base`, `crm`, `input`, `realization`, `epoch_map`, `probes`, `optimizer`, `emphasis`, `seed`, then optional `target_file`, `base_file`, `body_file`, `crm_file`, `transform_file`, `covariance_file`. Look and base are `slug@version`; CRM is `body@epoch@sim@version`. Values are percent-encoded. `looklab match` writes this string; `looklab verify --permalink` resolves the optional file fields relative to the permalink's directory and rejects an unsupported optimizer or a mismatched epoch map, transform version, or model version.

`looklab match --look <target.looklab> --body <schema.yaml> --crm <crm.json> --base-look <sim.looklab> --transform <transform.yaml> [--emphasis skin]` prints the settings card and the permalink. `--synthetic` substitutes the target Look as the base realization and the identity transform; it is required when those inputs are absent. `looklab verify --permalink <file> --observed <dir>` runs one loop iteration, prints the second suggestion's settings card, and writes an updated local CRM, a new permalink and a replay JSON carrying the second settings. Re-render at those settings and run `verify` with the new permalink to continue.

## 5. Verification loop (local, anonymous OK)

1. The app picks 2–4 of the user's RAWs (or asks for specific shots) covering the regions with the most predicted uncertainty.
2. The user re-renders them in camera at the recommended settings and drops in the JPEGs.
3. The client fits the observed realization, compares it with the prediction, updates a **local** copy of D (a few LM iterations) and re-optimizes. Repeat 1–3 times.
4. At the end, offer to contribute (this creates a credential if there isn't one). Verification renders are re-render series plus recipe data.

The first implementation updates one scale factor per non-default control with two Gauss-Newton iterations (ridge 1e-3, factor clamped to 0.05–20), then re-runs the full search. Selection ranks candidate RAW sample sets by mean `tr Cov_pred` at the recommended settings and keeps 2–4. Stored `in_rgb` is camera RGB before the body realization effects; `A_c · in_rgb` is the unmodified scene, and the realization applies WB and exposure once for prediction and target measurement. `looklab verify` reports the first suggestion's error against the observed re-renders; measuring the second suggestion needs a re-render at its settings.

## 6. Interface profiles (software pipelines)

`data/interfaces/<id>.yaml`. Interface profiles describe software pipelines that apply a LUT at a known point: raw developers and editors. Their authors submit profiles in the app (as YAML or through a form), together with a round-trip report generated at submission ([08 §4](08-ui.md)). Users pick the pipeline from a list. Hacked camera firmware does not use profiles; it implements the card format (§8).

```yaml
id: example-app/lut3d-v1
name: "Example editor 3D LUT module"
version: 1
bodies: [vendor/model-a]                 # empty = body-agnostic
insertion:
  point: camera_rgb_wb_linear            # see enum
  encoding:
    transfer: log2_shaper                # linear | log2_shaper | srgb | custom_1d
    shaper: { min_ev: -12, max_ev: 6, mid_gray: 0.18 }
    primaries: camera_native             # camera_native | rec2020 | xyz_d65 | srgb
    range: [0.0, 1.0]
  illuminant_handling: single            # single | dual_interpolated
accepts:
  - type: lut3d
    sizes: [17, 33]
    interpolation: tetrahedral           # trilinear | tetrahedral
    precision: f16                       # f32 | f16 | u16 | u12
    with_shaper: true
  - type: matrix3x3
  - type: curve1d
    points_max: 256
    per_channel: true
downstream:
  - output_transform: { primaries: srgb, transfer: srgb }
  - gamut_clip: hard
formats: [cube, haldclut, native_json]
limits: { max_bytes: 8388608 }
```

**`formats` (v1):** `cube`, `haldclut` (16-bit PNG), `native_json`.

**`insertion.point` (v1):**
- `camera_rgb_wb_linear`: after demosaic + WB, before any color matrix.
- `scene_linear_rec2020`: after the camera's color matrix.
- `scene_linear_xyz_d65`
- `display_encoded_post_tone`: partial looks only. The bake reports what's lost.

## 7. Baking

```
F = Downstream⁻¹ ∘ OutputEncode ∘ CAM16UCS⁻¹ ∘ L ∘ ToSceneXYZ_{P,B}(T) ∘ Decode_P
```

- `ToSceneXYZ`: for `camera_rgb_wb_linear` it's `A_B(T)`, which is **illuminant-dependent**:
  - `illuminant_handling: single`: bake at a chosen CCT (default D65; daylight 5500 K and tungsten 3200 K are selectable). The validation report shows the error at the other two CCTs.
  - `dual_interpolated`: bake two artifacts (StdA, D65). The pipeline interpolates in 1/T using its own CCT estimate.
  - Other insertion points use fixed matrices and are illuminant-independent.
- Out-of-gamut targets under a hard downstream clip get perceptual clipping (reduce chroma at constant J′, h in CAM16-UCS). The affected fraction is reported.
- If a shaper is accepted, choose it so the 3D grid is roughly uniform in `s`.

**Validation (required before export):** evaluate the artifact through a software model of the declared pipeline on the probe set and report ΔE vs. target (median, p95, max, plus the per-CCT results above). Exports with p95 > 1.0 ΔE outside unsupported regions are flagged.

**Writers:** `.cube` (with DOMAIN_MIN/MAX; shaper as a separate 1D file where needed, variant documented per profile), HaldCLUT (16-bit PNG), `native_json`.

Later: darktable `.dtstyle` (darktable as a software body) and RawTherapee profiles.

## 8. Firmware card format

A card file carries one Look version for hacked camera firmware. looklab defines the file and its semantics; it does not model any camera's internal pipeline. The firmware translates the file into whatever its pipeline lets it set (a native film-sim stage, a LUT stage, or both), so the only information lost is what that firmware can't set. One file serves every camera that implements this section.

Every published Look version includes `card.llk` as a manifest-listed artifact ([03 §5](03-data-model.md)). The writer is deterministic: the same Look version yields the same bytes.

### 8.1 Semantics (fixed in v1)
- **Look:** the family target θ_L, not any body's realization. Body parameters (Δ_c, ξ, ω, D, clip k) and control-response models are not in the file.
- **Input:** scene-linear Rec.2020 RGB after the camera's white balance, adapted to a D65 white (`wb_policy: adaptive`). Mid gray is 0.18, which is `s = 0` ([02 §3](02-color-pipeline.md)). The firmware maps its camera RGB into this space with its own color matrix, at its own CCT estimate; the illuminant handling of §7 does not apply.
- **Output:** the Look's appearance under the reference viewing conditions ([02 §1](02-color-pipeline.md)), converted J′a′b′ → XYZ → linear Rec.2020 RGB with Y = 1 at the reference white. Values are **pre-clip**: out-of-gamut targets keep their values, and the firmware's own output encoding and clip come after. Where the firmware can, out-of-gamut targets should be clipped perceptually (reduce M′ at constant J′ and h), as in §7.
- **Shaped coordinate:** the log2 shaper of `preview.lut33` ([03 §5](03-data-model.md)), applied per channel: `0.5 + 0.5 · log2(1 + x / 0.18) / log2(1 + 2⁶)` for `x ≥ 0` and `0.5 − 0.5 · log2(1 − x / 0.18) / log2(1 + 2¹²)` below.

### 8.2 Layout
Little-endian throughout. 8.3 file names; the export suggests `<NAME>.LLK` from the slug, and readers take identity from the header, never from the file name.

| Offset | Field |
|---|---|
| 0x00 | magic `LLCF` |
| 0x04 | `u16` format version = 1 |
| 0x06 | `u16` header length = 64 |
| 0x08 | `u32` file length |
| 0x0c | `u16` section count, `u16` = 0 |
| 0x10 | `u32` CRC-32 (IEEE) over the header and the section table, with this field taken as 0 |
| 0x14 | 32-byte content id: SHA-256 of every byte after the section table |
| 0x34 | 12 bytes, all 0 |
| 0x40 | section table: `count × 16` bytes |

Each section-table entry is `u32` kind, `u32` offset (4-byte aligned, inside the file), `u32` length, `u32` CRC-32 of the section's bytes. Entries are sorted by kind, with no duplicates and no overlaps. Readers ignore kinds they don't know. The manifest's SHA-256 covers the whole file; the content id lets firmware name the file in the render tag (§8.4) without hashing it.

### 8.3 Sections

| Kind | Name | Required | Content |
|---|---|---|---|
| 1 | `META` | yes | TLV records `u8` key, `u16` length, bytes. Keys: 1 slug (ASCII ≤ 63), 2 version (ASCII ≤ 31), 3 name (UTF-8 ≤ 63 bytes), 4 short name (ASCII ≤ 27), 5 description (ASCII ≤ 511), 6 license and attribution (UTF-8 ≤ 255 bytes). Keys 1-4 are required; unknown keys are skipped. |
| 2 | `SEMANTICS` | yes | `u16` fields: input space (1 = §8.1), output space (1 = §8.1), shaper (1 = §8.1), viewing conditions (1 = reference), `wb_policy` (1 = adaptive), then `u16` = 0 padding. v1 readers reject any other value. |
| 3 | `NEUTRAL` | yes | `u32` n = 257, then n × 3 `f32`: the Look's output for a neutral input at `s = −12 + 18·i/(n − 1)`. This is the full Look on the neutral axis (T, N and the `C = 0` plane of Λ), evaluated exactly. It maps directly onto a native tone curve. |
| 4 | `FIT` | yes | `u32` n, then n records of 40 bytes: input `f32×3`, output `f32×3`, `σ_ΔE` `f32`, weight `f32`, flags `u32` (bit 0 = supported), probe index `u32`. v1 records are the probe set of the Look version, in probe-set order. Weight is the probe's `weight` from the probe set for supported probes and 0 otherwise, normalized to sum 1. |
| 5 | `GRID` | no | `u32` side (33 or 65), then side³ × 3 `f16`, red fastest, over the shaped input coordinate. Values are §8.1 output. A 65³ grid is about 1.6 MB; the other sections together are under 50 KB. |

**Grid interpolation (normative):** clamp each shaped coordinate to [0, 1], split it into a base node and fractions. Sort the fractions descending, ties broken red before green before blue, to get `f₁ ≥ f₂ ≥ f₃` with unit steps `e₁, e₂, e₃`. Then `out = (1 − f₁)·V₀ + (f₁ − f₂)·V(e₁) + (f₂ − f₃)·V(e₁ + e₂) + f₃·V(e₁ + e₂ + e₃)`.

**Reader rules:** reject the file unless the magic, version, lengths, header CRC and the CRCs of every section it reads match; every required section is present; counts match lengths; and every value it reads is finite. A rejected file changes nothing: the firmware keeps its previous state and never uses a partly read file. Sections the firmware doesn't use (typically `GRID`) need not be read.

### 8.4 Render tag
Firmware **must** write this token, in ASCII, into EXIF `UserComment` of every JPEG it renders with a card look:

```
looklab-card/1 fw=<firmware-id>/<firmware-version> look=<slug>@<version> cid=<first 8 bytes of the content id, lowercase hex>
```

`firmware-id` is `[a-z0-9-]{1,32}`; `firmware-version` is `[0-9A-Za-z.+-]{1,24}`. The sanitizer keeps only the parsed fields ([06 §1](06-privacy-and-payload.md)). A tagged image is a card render: it becomes an intent-`card` contribution ([07 §4](07-contribution.md)) and never enters a Look or body fit. Without the tag, a card render reads as a stock render of whatever sim the firmware borrowed and would contaminate that sim's fit; outlier scoring (z > 3.5) is the only backstop. Card renders are grouped by firmware id and version, so a poor translation shows up against the firmware, not the Look. When the tag is missing, the user picks the firmware from a list.

The client checks `cid` against the published `card.llk` of that version. A mismatch means the card holds a different file; the image isn't counted for that version, and the user is told.

### 8.5 Translation guidance (non-normative)
looklab can't see camera internals, so this is advice, not a requirement.
- **Translate on camera at startup.** It needs no host tool. Cache the result on the card keyed by the content id and refit only when the id changes, so only the first boot after a change pays for the fit.
- **Fit from `FIT` and `NEUTRAL`, not the grid.** They stream in small fixed buffers, which suits firmware without an allocator.
- **Stage the fit like looklab does** ([02 §4.1](02-color-pipeline.md)): tone and neutral parameters from `NEUTRAL` first, then color parameters on `FIT` with tone held.
- **Loss:** weighted squared error in CIELAB or CAM16-UCS, with weight `w / (1 + σ²)`, compared after the camera's own output encoding and clip.
- **Offline fallback:** where startup time or the FPU can't afford the fit, a host tool can read the file and write the firmware's native format.

### 8.6 Conformance
`data/card/conformance-v1/` holds valid and malformed files with the expected reader outcome, grid-evaluation vectors (shaped input → output under the normative interpolation), and render-tag strings with their parse result. Grid outputs must match the reference within 2 f16 ulp *(tentative)*.

## 9. Reference implementation

The reference consumer is looklab's own card reader and grid evaluator in `looklab-export`, native and wasm32, exposed as `looklab-cli card inspect | eval`, plus the conformance vectors of §8.6. An example translator in portable C99 with no allocator fits a 3×3 matrix and a per-channel tone curve from `FIT` and `NEUTRAL`, for firmware authors to start from. Software interface profiles keep their own bake → simulate → compare round-trip test in `data/interfaces/` with fixtures.
