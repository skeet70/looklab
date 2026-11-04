# 02 — Color pipeline

This doc defines what a Look is, how it's estimated from RAW+JPEG pairs across bodies, how uncertainty is computed, and how looks are compared.

**Notation:**

| Symbol | Meaning |
|---|---|
| `rgb_cam` | Linear camera RGB, white-balanced |
| `XYZ` | CIE 1931 2° tristimulus |
| `J'a'b'`, `M'`, `h` | CAM16-UCS lightness, Cartesian chroma axes, colorfulness, hue |
| `s` | Scene exposure, `log2(Y/0.18)` |
| `u` | Control settings vector |
| `T, N, G, R, Λ` | Look layers: tone, neutral tint, chroma gain, hue rotation, lightness shift (Λ, to avoid CAM16's brightness Q) |
| `F₁, F₂` | Two functions being compared (§7); `H` is reserved for the Hessian |

## 1. Forward model

For image *i* from contributor *v* on body *c*, epoch *e*, rendered with native sim *k* at settings *u*:

```
J'a'b'_jpeg(i,p) ≈ Clip_k( Look_{θ(k,u)}( A_c(T_i) · diag(ω_c · w_i) · 2^(ξ_{c,m} + x_i) · rgb_cam(i,p) ) ) + ε
```

- `rgb_cam(i, p)`: black-subtracted, white-normalized, **as-shot WB applied**.
- `ξ_{c,m}`: **body-level exposure offset** per body *c* and exposure mode *m* (DR mode, or an ISO band where DR expansion engages). It's a realization parameter: the settings optimizer sees it and can recommend exposure compensation ([04 §3](04-matching-and-export.md)). Gauge: see §2.1.
- `x_i`: per-image exposure nuisance around ξ. Prior `x_i ~ N(x_meta, 0.3²)`, where `x_meta` comes from known exposure tags (else 0).
- `ω_c`: **body-level WB offset** (2 DOF, R/B gains vs G), per body and WB mode class (auto / preset / custom). It's a realization parameter. Prior `log ω_c ~ N(0, 0.03²)` per channel; gauge in §2.1.
- `w_i`: per-image WB nuisance around ω. Prior `log w_i ~ N(0, 0.02²)`.
- `A_c(T_i)`: body input transform at the image's correlated color temperature (§2).
- `Look_θ`: Look layers with coefficients θ (§3). `θ(k, u)` is the realization of native sim *k* at settings *u* (§4.2 and [04 §2](04-matching-and-export.md)).
- `Clip_k`: output encoding with soft clipping (§3.4).
- `ε`: multivariate Student-t (ν = 4) with **per-contributor scale σ_v** (§4.1).

The JPEG side is decoded per its declared color space (sRGB or Adobe RGB), converted to XYZ, then to CAM16-UCS under the **reference viewing conditions**:

| Parameter | Value |
|---|---|
| Adopted white | D65 |
| L_A | 64/π × 0.2 ≈ 4.07 cd/m² |
| Y_b | 20 |
| Surround | Average |
| **Degree of adaptation** | **D = 1 (discount the illuminant)** |

D = 1 is required. Inputs are already D65-adapted and the display white is D65. With D computed from L_A (≈ 0.832), every neutral lands at |a′b′| ≈ 0.6–2.2 toward h ≈ 210°, which contaminates the tint and chroma layers. The conditions are recorded in every Look file. All comparisons must use identical conditions.

**Why adapted scene XYZ?** Native sims run after white balance. Defining the Look on WB'd colors makes it illuminant-independent, which matches how sims behave. Fixed-cast looks are handled by `wb_policy` (§3.7).

## 2. Body input transform `A_c(T)`

**Dual illuminant.** The camera→XYZ mapping depends on the illuminant. Each body has two matrices, `A_c^A` (≈ 2856 K) and `A_c^{D65}`. They're interpolated linearly in 1/T (DNG convention) at the image's CCT `T_i`:
- `T_i` is estimated from the as-shot neutral (the inverse of the WB multipliers) with the body's color-matrix pair, using the iterative DNG method.
- The client computes `T_i` as a hint. **The server recomputes it** from the multipliers in the sample header.

**Parameterization:** `A_c^X = A_c^{X,0} · (I + Δ_c)` for X ∈ {A, D65}, with **one shared Δ_c** (v1).
- Δ_c **preserves white**, `(I + Δ_c)·1 = 1`, which leaves 6 DOF.
- Prior means `A_c^{X,0}`, in source priority order:
  1. spectral-sensitivity-derived matrices;
  2. DNG ForwardMatrix1/2 (else inverted ColorMatrix1/2, adapted to D65) from that body;
  3. a camera database matrix (rawler / LibRaw / darktable), which may be single-illuminant. Then both X use it, with a looser prior.
- Prior `vec(Δ_c) ~ N(0, Σ_src)`: tight for SSF or chart-anchored bodies, loose for database-only. The six parameters are the free entries of `I + Δ`; `Σ_src` is isotropic in that parameterization:

  | `a0_source` | standard deviation per parameter *(tentative)* |
  |---|---|
  | `ssf` | 0.002 |
  | `dng` | 0.01 |
  | `db` | 0.04 |
  | `chart` | 0.005 |

  `looklab-body` sets these as `SSF_PRIOR_SD`, `DNG_PRIOR_SD`, `DB_PRIOR_SD` and `CHART_PRIOR_SD`; an anchored body replaces `Σ_src` with the chart posterior covariance.
- **Chart anchoring:** chart patches give `(rgb_cam, XYZ_ref)` pairs at a known (estimated) CCT. They enter as a separate likelihood term and don't pass through any Look. Only chart sessions from endorsed or seeder credentials tighten the prior and define the gauge; new-tier charts are ordinary data ([05 §5](05-trust-and-integrity.md)).
- **v2:** a separate Δ per illuminant; a root-polynomial term, gated on chart data.

**Gauge (global).** A shared 3×3 could move between the body transforms and every Look's input side. The gauge is fixed **globally**, not per Look:
1. Chart- or SSF-anchored bodies have tight priors. They define the gauge.
2. If no body is anchored at all (early bootstrap), add a soft constraint that the weighted mean of Δ over all bodies is 0.

Δ_c belongs to the body. It's estimated **across all Looks native on that body** (§4.3), not per Look.

### 2.1 Exposure and WB gauges
ξ and ω belong to bodies and are shared by every Look a body produces. So their gauges are fixed **once per connected component** of the body–Look graph (bodies linked by Looks they both realize), never per Look:
- **Exposure:** ξ = 0 for **one designated body in one exposure mode** (its default mode, e.g. DR100), per component. The designated body is **chosen once** (initially the body with the most root data) and recorded in provenance. It changes only through a logged moderator action, like a re-anchor. Every other (body, mode) pair, **including the designated body's other modes** (e.g. its DR200/DR400 shifts), is free.
- **WB:** ω = 1 for the **same designated body's** default WB mode class, per component. Other bodies' ω_c are free, with the prior from §1. A WB rendering shared by the bodies (e.g. a sim's intended warmth) is then carried by the Look's tint layer N, and a body-specific WB bias by ω_c.
- **Bodies joining or leaving a component move neither gauge**, so nothing needs converting. Gauges change only when components **merge** (two components gain a shared Look) or a moderator **re-designates** the reference body. Each change is recorded in provenance.
- **Handling a gauge change before gating** ([05 §8](05-trust-and-integrity.md)):
  - **Exposure: exact conversion.** A scalar exposure scale commutes with every body's input transform, so the old Look in the new gauge is `L'(XYZ) = L(2^Δξ · XYZ)`. Evaluate the old Look on rescaled probe inputs; no refit is needed. One caveat: the s-clamp then sits at 6 − Δξ, which only affects the top gray-ramp probes when |Δξ| > 1. Exclude those from the gate for that release.
  - **WB: refit + re-anchor.** A WB gain in camera space corresponds to `A_c · diag(Δω) · A_c⁻¹` in scene space, which differs per body. So no single transform of a shared Look reproduces a WB gauge change, and the WB constraint is only a near-degeneracy with N, not an exact symmetry. A WB reference change therefore triggers a full refit, and the result becomes a **logged re-anchor**: its gates compare against the previous version only for root non-regression, and a moderator approves it.

## 3. Look representation

A Look maps adapted scene XYZ to CAM16-UCS `J'a'b'`. Input coordinates:

- `s = log2(Y/0.18)`, clamped to [−12, +6].
- `(h_in, C_in)`: hue and chroma of the input chromaticity, computed by scaling XYZ to Y = 0.18 and converting to CAM16-UCS. They're independent of exposure, and they're used **only as spline coordinates**.

### 3.1 Tone `T`
`J'_base = T(s)`. Monotone non-decreasing spline (I-spline), ~20 knots over [−12, +6], dense in [−4, +3], including the shoulder.

### 3.2 Neutral tint `N`
`(a'_N, b'_N) = N(J'_base)`, two smooth splines (~12 knots). With D = 1, a neutral input gives `M'_ref = 0` (§3.3), so N measures neutral tint exactly.

### 3.3 Chroma, hue and lightness relative to a colorimetric reference
To separate tone from color, G, R and Λ are defined **relative to what a purely colorimetric rendering would produce at the neutral-matched luminance**:

1. Find `Y_ref`, the luminance of the neutral whose CAM16-UCS lightness equals `T(s)` (closed form). `XYZ_ref` is the input chromaticity at luminance `Y_ref`; `(J′_ref, M′_ref, h_ref)` are its colorimetric CAM16-UCS values.
2. Layers over `(h_in, s, √C_in)`:
   - chroma gain `log G`;
   - hue rotation `R` (degrees);
   - lightness shift `Λ` (J′ units).
3. Output:
   ```
   J'   = J'_ref + Λ(h_in, s, C_in)
   M'   = G(h_in, s, C_in) · M'_ref
   h    = h_ref + R(h_in, s, C_in)
   a'b' = N(J'_base) + polar(M', h)
   ```

T is the neutral-axis tone curve. Chromatic colors keep their colorimetric lightness relative to the neutral of the same luminance.

With colorimetric `T` (`T(s)` = neutral `J'` at `Y = 0.18·2^s`), `Y_ref = Y` and the output equals colorimetric CAM16-UCS of the input.

**G = 1, R = 0, Λ = 0 means "tone change only, otherwise colorimetric".** Two looks that differ only in tone report identical G/R/Λ.

Splines are tensor-product B-splines: **periodic cubic in hue (24 knots)** × cubic in s × cubic in √C (4 knots). The **s-knots (10) are a subset of the tone knot vector**: dense where T is dense, including the shoulder, so highlight desaturation is representable. At `C_in = 0` the hue axis has no meaning, so the `C = 0` coefficient plane is shared across hue and G/R/Λ are hue-independent there.

### 3.4 Output encoding and soft clip
The Look predicts appearance before output encoding. To compare with JPEGs: J′a′b′ → XYZ → linear output RGB (the JPEG's primaries) → per-channel **soft clip** → back to XYZ → CAM16-UCS J′a′b′, where residuals are measured. (The JPEG side is decoded through its TRC to linear first, so no TRC round-trip is needed on the prediction side.)

```
c(x) = x                                   for k ≤ x ≤ 1−k
c(x) = (1−k) + k·tanh((x − (1−k))/k)       for x > 1−k
c(x) = k − k·tanh((k − x)/k)               for x < k
```

The function is C¹ and maps (−∞, ∞) → (0, 1). `k` is a per-realization parameter (default 0.03, fitted per body/sim; prior N(0.03, 0.02²) truncated to [0.005, 0.1]). The Look file stores the function id and `k` for each realization in its provenance. The published Look itself is pre-clip.

Pre-clip values in saturated regions are only weakly constrained by data, because the clip's derivative → 0. This shows up as large posterior variance, so those probes are correctly marked unsupported (§5).

### 3.5 Residual LUT
A 17³ grid over `(s, a_in, b_in)` outputs ΔJ′a′b′, trilinear. Regularization: Laplacian smoothness + L2. Nodes with negligible density are pinned to 0. A large residual is itself a diagnostic.

### 3.6 Coverage / density
A kernel density estimate of the training samples in `(s, h_in, √C_in)`, weighted by trust weights. Used for support (§5) and bounties.

### 3.7 Look metadata
`wb_policy`:
- `adaptive` (v1): applies to WB'd colors.
- `fixed_illuminant`: later.

Also: viewing conditions, probe set version, kind, provenance (including input-transform, control-response-model and anchor versions; [03 §3](03-data-model.md)).

## 4. Fitting

### 4.1 Objective and noise model
Per-sample negative log-likelihood (multivariate Student-t, ν = 4, 3-D residual r):

```
ℓ_ip = ((ν+3)/2) · log(1 + ||r_ip||² / (ν σ_v²)) + 3 log σ_v
```

The total objective is:

```
Σ_i  (n_eff / n_i) · Σ_p  ω_ip · ℓ_ip
  + smoothness/shrinkage terms (λ_T, λ_N, λ_GRΛ, λ_res)
  + priors (x_i, w_i, ξ, ω, Δ_c, k, firmware offsets, σ_v)
  + chart term
```

- **σ_v appears only here**, including its log term, so inflating it isn't free. It's **not** repeated as a weight factor ([05 §5](05-trust-and-integrity.md)). Prior: inverse-gamma on σ_v², centered at 1.5 ΔE.
- **Tempering:** samples within an image are strongly correlated. The factor `n_eff/n_i` (default n_eff = 500) keeps one image from overwhelming the priors and nuisance terms. Uncertainty is still computed with the sandwich estimator (§5).
- `ω_ip` = density weight × the image's trust weight from 05; the stored `density_w` is the raw inverse inclusion probability, normalized to mean 1 within each image so it reweights samples without changing the image's total weight.
- Fitting uses up to 10k samples per image by default (a uniform draw within the stored ≤ 50k set).

**Staging** (tone first, after dtsolve's practice). Each stage warm-starts the next:
1. T, x_i (and ξ) on near-neutral samples (`C_in` small; well-defined because D = 1).
2. Add N.
3. Add G, R, Λ.
4. Fit the residual on the residuals, then do a short joint refinement.

**Regularization:** choose each λ **within its own stage** by generalized cross-validation (GCV) on that stage's linearized problem, using a 1-D search over ~8 log-spaced values. K-fold CV over images is a periodic quality check on a per-image subsample, not part of every fit. Store the chosen λ in the Look file.

Solver: Levenberg–Marquardt with sparse normal equations (`faer`).

### 4.2 Joint Look + control-response fit
**Reference setting:** every control marked `look_affecting: true` in the body's schema is at its declared `default`. This is the single definition of "target-defining" ([03 §1](03-data-model.md)). Spatial controls (grain, clarity, sharpening, NR) must be at default or at a schema-listed tolerated value for an image to be used at all.

All native images on a body are used, **at any settings**. θ (the Look coefficients the body's sim realizes) and that body's control-response coefficients D ([04 §2](04-matching-and-export.md)) are fitted **jointly**:

```
θ(k, u) = θ_L + δ_{fw} + Σ_j φ_j(u_j) D_{c,k,j} + interactions
```

So non-default library images inform θ through the shared θ_L, rather than being thrown away.

### 4.3 Global hierarchical fit (multi-body)
The unknowns:
- per Look family: θ_L;
- per body: Δ_c, ξ_{c,m}, ω_c, D_{c,k}, clip k_{c,k};
- per firmware version: small offsets δ_fw on T and G coefficients (tight priors);
- per contributor: σ_v;
- per image: nuisances.

Each refit runs in **two stages**, each minimizing a single well-defined objective:

1. **Native stage.** Use native data only (all settings) plus chart terms. Block coordinate descent to convergence:
   - (a) per-body parameters (Δ_c, ξ, ω, **D**, clip k) given all θ_L;
   - (b) per-Look θ_L (and δ_fw) given body parameters.

   D appears in block (a) only.
2. **Recipe stage.** Hold every θ_L fixed. Refit the per-body parameters (Δ_c, ξ, ω, D, clip k) on native + recipe data, warm-started from stage 1. These are the published body parameters.

**Recipe images contribute nothing to any θ_L**, not just to the Look they attempt. Splitting into two stages (rather than letting recipe data into block (a) of a single alternation) keeps each stage a true minimization, so it converges instead of oscillating between two objectives.

**Warm starts:** each native stage starts from the **previous native-stage solution**, never from the published recipe-stage parameters. Otherwise recipe data could still influence which local optimum the Looks land in.

**Refit cascade:**
- An update to a body (new chart session, new data) marks the body dirty.
- A nightly batch runs the native stage (blocks (a) and (b) for dirty bodies and every Look native on them), then the recipe stage for those bodies, then weighting and release gates ([05 §8](05-trust-and-integrity.md)).
- Look provenance records, for every contributing body, the input-transform version **and the native-stage body parameters the Look was fitted against**. Those are never published as a body version otherwise.

### 4.4 Outlier scoring (per image)
For each image: weighted robust RMS ΔE against the prediction, then a robust z-score within the pool (Look, body, epoch) using median/MAD.
- `z > 3.5` → quarantine.
- `2.5 < z ≤ 3.5` → down-weight.

Reason heuristics from residual structure:
- exposure-like → DR/exposure mismatch;
- hue-uniform cast → unrecorded WB shift;
- shoulder-only → highlight/DR setting;
- content-correlated → scene-adaptive pipeline.

### 4.5 Epoch splitting
For adjacent firmware versions *a*, *b* in an epoch, test at the **image level**:
1. Compute each image's projection of its residual onto the firmware-offset basis (one vector per image).
2. Run a two-sample permutation test on Hotelling's T² between the versions' image sets (10⁴ permutations).
3. Split if p < 0.001 **and** the estimated effect is ≥ 1.0 ΔE median over supported probes.

A positive result opens a moderation proposal.

## 5. Uncertainty

Per Look version, per-probe 3×3 predictive covariance:

1. **Fit (interactive path):** an image-clustered **sandwich estimator** `Σ_θ = H⁻¹ (Σ_i g_i g_iᵀ) H⁻¹`, where `g_i` is image *i*'s summed gradient at the optimum. It's propagated to probes through the Jacobian. Optionally cluster by contributor for an even more conservative estimate.
2. **Bootstrap (published versions):** B = 32 warm-started refits on images resampled with replacement.
3. **Cross-body disagreement:** covariance of leave-one-body-out predictions.

Combination: `Σ_fit = Σ_boot` when available, else `Σ_sandwich`, then `Σ_total = Σ_fit + Σ_disagree`. Scalar `σ_ΔE = sqrt(tr Σ_total)`.

**Support:** a probe is *supported* iff `σ_ΔE ≤ 1.5` **and** density ≥ the floor. This automatically excludes pre-clip values the data can't identify (§3.4). Unsupported probes are excluded from matching objectives and hatched in plots.

## 6. Probe set (v1)

Versioned in `data/probes/vN.yaml`, generated deterministically, in adapted scene XYZ (D65).

| Group | Contents | Weight |
|---|---|---|
| Gray ramp | s ∈ [−10, +5], step 0.25 | 1.0 |
| Chart patches | ColorChecker 24 + SG 140 reflectances under D65 at s ∈ {−2, −1, 0, +1, +2} | 1.0 |
| Hue grid | 24 hues × 4 chroma levels × s ∈ {−5..+3}. Chroma levels are 20/40/60/80% of the Pointer's-gamut boundary in CAM16-UCS `M'` **at the Y = 0.18-normalized chromaticity** (L* ≈ 49.5, inside the tabulated range), at the boundary's CAM16 hue. The same chromaticity is used at every s. | 0.5 |
| Skin | Skin-tone reflectance set (TBD, [10 Q5](10-open-questions.md)) at s ∈ {−1, 0, +1} | 2.0 |
| Memory colors | Sky, foliage and similar | 1.5 |

## 7. Diagnostics and comparison

For two functions F₁ and F₂ on the probe set (Look vs Look, Look vs realization, target vs achievable), report:

1. Tone: J′ vs s, and the difference.
2. Neutral tint: a′, b′ vs J′.
3. Chroma gain by hue: G vs h at s = 0, mid chroma; small multiples over s.
4. Hue rotation by hue: R vs h.
5. Lightness shift by hue: Λ vs h.
6. ΔE (CAM16-UCS) distribution: median, p95, max, worst 10. Uncertainty-weighted variants use weight `1/(1 + σ²_F₁ + σ²_F₂)`, normalized.
7. Region map: ΔE over (h, s) at mid chroma, with unsupported regions hatched.
8. One-line natural-language summary built from layer differences.

Because G/R/Λ are relative to the colorimetric reference, layer differences are directly interpretable. CIEDE2000 is also reported for familiarity.

## 8. Implementation notes

- f64 for all fitting and color math; f32 only in sample storage.
- **Determinism (native = WASM bit-identical):**
  - All transcendental math in core crates goes through the `libm` crate (`pow`, `exp`, `ln`, `sin`, `cos`, `atan2`, `tanh`, …). Ban the `f64` methods with clippy `disallowed_methods`. `sqrt` (correctly rounded) and basic arithmetic are fine.
  - The Look evaluation path uses scalar code or fixed-order reductions only: no `faer`/`pulp` or other runtime-dispatched SIMD, no fast-math, no FMA contraction differences. A test compares native and WASM outputs bit for bit on fixtures.
- Golden tests vs colour-science for every color function (1e-6 relative; CAM16 inverse 1e-5). Note: colour-science's CAM16 defaults compute D, so golden vectors must pass `discount_illuminant=True` where the reference conditions apply.
- Look evaluation target: < 3 µs/color native for layers + residual, including the closed-form `Y_ref` solve (§3.3).
