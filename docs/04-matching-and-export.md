# 04 — Matching and export

Given a user's body *B* (epoch *e*) and a target Look *L*, produce either:
- **Settings** for stock firmware: a base native sim, control values, and exposure compensation / WB shift where useful; or
- **An artifact** for firmware that can load one, per a declared interface profile.

Probes are defined in adapted scene XYZ, so the target on any body is `L(p)`. The body enters through its realization parameters (stock path) or through the bake (artifact path).

## 1. Matching levels

| Level | User provides | Output |
|---|---|---|
| A | nothing / RAWs | Target renders of the user's RAWs (via `A_B` + L) |
| B | RAW+JPEG from their current setup | Fitted realization of the current setup + diagnostic comparison vs. target |
| C | body only (uses the crowd model); optional verification loop | Recommended settings + predicted error map + unreachable regions |
| C+ | firmware interface profile | Baked artifact + validation report |

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

Large nonlinear moves, such as a two-stop tone change together with exposure compensation, can be pruned by a linearization at default. The leading sim therefore gets a coarse exact coordinate descent on 64 probes, plus two random-restart descents on 48 probes, and the beam seeds are exact-scored on the full set. On 3 synthetic sims and the 1057-probe set the native release search measures median 0.85 s, p95 1.14 s; the same example under wasm32-wasip1 on wasmtime measures median 1.06 s, p95 1.38 s.

## 4. Settings card and permalinks

The **settings card** shows:
- The base sim and every control value, in the camera's menu order and wording, **including exposure compensation and WB shift** when they help (e.g. "this body renders 0.5 EV darker in DR200: set EC +0.3 and WB shift R+1").
- Predicted ΔE median / p95 with the uncertainty band.
- A per-layer residual summary in natural language.
- Unreachable regions, e.g. "Cyan chroma can't be reduced enough; the remaining error is 3.8 ΔE."
- Confidence, which is low when the control-response model is thin.

**Permalinks** pin everything needed to reproduce the result: Look version, base realization Look version, control-response model version, input transform version, realization params version, epoch map version, probe set version, optimizer version and the user's emphasis. All versions are immutable, so a permalink always reproduces.

The canonical encoding is a single line of `key=value` fields joined by `|`, starting with `looklab-match/1`, in this order: `look`, `base`, `crm`, `input`, `realization`, `epoch_map`, `probes`, `optimizer`, `emphasis`, then optional `target_file`, `base_file`, `body_file`, `crm_file`, `transform_file`, `covariance_file`. Look and base are `slug@version`; CRM is `body@epoch@sim@version`. Values are percent-encoded. `looklab match` writes this string; `looklab verify --permalink` resolves the optional file fields relative to the permalink's directory.

`looklab match --look <target.looklab> --body <schema.yaml> --crm <crm.json> [--emphasis skin]` prints the settings card and the permalink; `looklab verify --permalink <file> --observed <dir>` runs one loop iteration and prints the measured error of the first suggestion and the re-optimized second suggestion.

## 5. Verification loop (local, anonymous OK)

1. The app picks 2–4 of the user's RAWs (or asks for specific shots) covering the regions with the most predicted uncertainty.
2. The user re-renders them in camera at the recommended settings and drops in the JPEGs.
3. The client fits the observed realization, compares it with the prediction, updates a **local** copy of D (a few LM iterations) and re-optimizes. Repeat 1–3 times.
4. At the end, offer to contribute (this creates a credential if there isn't one). Verification renders are re-render series plus recipe data.

The first implementation updates one scale factor per non-default control with two Gauss-Newton iterations (ridge 1e-3, factor clamped to 0.05–20), then re-runs the full search. Selection ranks candidate RAW sample sets by mean `tr Cov_pred` at the recommended settings and keeps 2–4. `looklab verify` reports the first suggestion's error against the observed re-renders; measuring the second suggestion needs a re-render at its settings.

## 6. Firmware interface profiles

`data/interfaces/<id>.yaml`. Firmware authors submit profiles in the app (as YAML or through a form), together with a round-trip report generated at submission ([08 §4](08-ui.md)). Photographers just pick their firmware from a list.

```yaml
id: example-firmware/sim-v1
name: "Example firmware sim slot"
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
formats: [cube, native_json]
limits: { max_bytes: 262144 }
```

**`formats` (v1):** `cube`, `haldclut` (16-bit PNG), `binary_f16` (packed half floats, layout in §7), `native_json`.

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
  - `dual_interpolated`: bake two artifacts (StdA, D65). The firmware interpolates in 1/T using its own CCT estimate.
  - Other insertion points use fixed matrices and are illuminant-independent.
- Out-of-gamut targets under a hard downstream clip get perceptual clipping (reduce chroma at constant J′, h in CAM16-UCS). The affected fraction is reported.
- If a shaper is accepted, choose it so the 3D grid is roughly uniform in `s`.

**Validation (required before export):** evaluate the artifact through a software model of the declared pipeline on the probe set and report ΔE vs. target (median, p95, max, plus the per-CCT results above). Exports with p95 > 1.0 ΔE outside unsupported regions are flagged.

**Writers:** `.cube` (with DOMAIN_MIN/MAX; shaper as a separate 1D file where needed, variant documented per profile), HaldCLUT (16-bit PNG), `binary_f16`, `native_json`.

`binary_f16` is what a firmware with `precision: f16` loads: 16 bytes of little-endian header (magic `LLLUT`, version, flags with a shaper-present bit, grid side as `u32`, shaper length as `u32`), then the shaper as half floats when present, then `size³ × 3` half floats in `.cube` order. A 33³ grid is about 1.0 MB and a 65³ grid about 1.6 MB, against 1.0 MB and 7.9 MB as `.cube` text.

Later: darktable `.dtstyle` (darktable as a software body) and RawTherapee profiles.

## 8. Reference implementation

The project owner's firmware is the first interface profile and the reference consumer. Its profile, a bake → simulate → compare round-trip test, and a sample artifact live in `data/interfaces/` with fixtures.
