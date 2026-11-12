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
- `x_i`: per-image exposure nuisance around ξ. Prior `x_i ~ N(x_meta, 0.3²)`. `x_meta` is a **relative** exposure offset against the body's exposure gauge, not an absolute exposure value: `rgb_cam` is image-referred, so a metered capture needs no absolute-luminance offset. The sample header's `exposure_hint_ev` is an EV reading at ISO 100 ([03 §2.2](03-data-model.md)), so it is not `x_meta`; call sites set `x_meta = 0`.
- `ω_c`: **body-level WB offset** (2 DOF, R/B gains vs G), per body and WB mode class (auto / preset / custom). It's a realization parameter. Prior `log ω_c ~ N(0, 0.03²)` per channel; gauge in §2.1.
- `w_i`: per-image WB nuisance around ω. Prior `log w_i ~ N(0, 0.02²)`.
- `A_c(T_i)`: body input transform at the image's correlated color temperature (§2), evaluated per image from both illuminant matrices; no D65-only substitute is used.
- `Look_θ`: Look layers with coefficients θ (§3). `θ(k, u)` is the realization of native sim *k* at settings *u* (§4.2 and [04 §2](04-matching-and-export.md)).
- `Clip_k`: output encoding with soft clipping (§3.4).
- `ε`: multivariate Student-t (ν = 4) with **per-contributor scale σ_v** (§4.1).

The target is one sensor-independent family Look, with coefficients `θ_L`. The camera terms in the formula are its realization, not a second target: shared input calibration `A_c`, body exposure and WB offsets `ξ`/`ω`, and the sim's control response and clip. A realization's identity is the subject `BodyRealization(body, epoch, native_sim)` plus an explicit revision that pins the family `look@version`, the input-transform version, the realization-parameter version, the control-response version and the clip ([03 §1](03-data-model.md)). Independent camera-local fits are diagnostics for cross-body agreement, never canonical targets. A match may target another native family or a `lut_defined` target; each sim's base realization still pins its own native-family version. Implementation rationale is in [model qualification plan §3](model-qualification-plan.md#3-model-subjects-and-artifact-identity).

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

The prior source is an approved, pinned server-side profile (SSF, DNG, database matrix or chart anchor) with persisted source identity, version and uncertainty. A local decoder's matrices are not a server profile, and the sample header carries no decoded matrix or calibration provenance. A missing approved source blocks only the computations that require server-authoritative calibration; it does not by itself block intake or preparation when the actual preparation inputs are available. A local decoder profile may prepare samples without establishing server authority. Report the specific preparation or fitting blocker, and do not invent an identity transform or add an upload field ([03 §1](03-data-model.md), [model qualification plan §7](model-qualification-plan.md#7-resources-and-broad-camera-support)). A realization revision pins the transform version, while fitting evaluates `A_c(T_i)` per image at the server-recomputed CCT.

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
  - **Exposure: exact conversion.** A scalar exposure scale commutes with every body's input transform, so the old Look in the new gauge is `L'(XYZ) = L(2^Δξ · XYZ)`. Evaluate the old Look on rescaled probe inputs; no refit is needed. The lookup saturation then sits at 6 − Δξ, which only affects the top gray-ramp probes when |Δξ| > 1; exclude those from the gate for that release.
  - **WB: refit + re-anchor.** A WB gain in camera space corresponds to `A_c · diag(Δω) · A_c⁻¹` in scene space, which differs per body. So no single transform of a shared Look reproduces a WB gauge change, and the WB constraint is only a near-degeneracy with N, not an exact symmetry. A WB reference change therefore triggers a full refit, and the result becomes a **logged re-anchor**: its gates compare against the previous version only for root non-regression, and a moderator approves it.

## 3. Look representation

A Look maps adapted scene XYZ to CAM16-UCS `J'a'b'`. Input coordinates:

- `s = log2(Y/0.18)` is physical exposure and is not clamped.
- `s_L = E(s)` is the bounded spline coordinate: identity on the tone range `[−12, +6]`, with C² quintic Hermite transitions outside it over `[−14, −12]` and `[6, 7]`, saturating at `−14` and `7`. The widths are `basis::S_SHADOW_TRANSITION = 2` and `basis::S_HIGHLIGHT_TRANSITION = 1` stops; `E` and `E'` are `basis::exposure_lookup` and `basis::exposure_lookup_derivative`. The tone spline, G/R/Λ and the residual use `s_L` and extrapolate their end cubic pieces outside the knot range, so the composite is C²; the colorimetric tone uses physical `s`.
- `(n_a, n_b)`: the pre-power cone-opponent coordinates of the input chromaticity at `Y = 0.18`, normalized as `n = K·(a, b)/D` with `a = R'a − 12G'a/11 + B'a/11`, `b = (R'a + G'a − 2B'a)/9`, `D = R'a + G'a + 21B'a/20`, and `K = C0·D_ref/ρ0` from the reference viewing conditions (`K = 1204.4924`). They are C^∞ in the cone responses, linear at the origin with an isotropic Jacobian, carry neither the hue eccentricity factor `e_t(h)` nor the lightness factor, and vanish on the neutral axis. They are the core input, the near-neutral reference coordinates and the near-neutral residual coordinates. Derivation: `local://opponent-coords.md`.
- `(h_in, C_in)`: hue and chroma of the same input in CAM16-UCS. They index the outer hue and `√C` axes only.

The domain is defined by predicates on the actual failing quantities, not on RGB signs: `Y > 0`; the pre-compression cone responses strictly away from zero, where the post-adaptation `0.42` power has a vertical tangent; the temporary-magnitude denominator `D > 0`; and the inverse's adapted responses strictly inside `|R'a − 0.1| < 400`, with the neutral `J'` limit `solve::neutral_j_prime_max()` derived from the actual inverse and the viewing conditions. `domain_margin` and `prediction_margin` return a signed distance that is positive strictly inside. The fit fixes its eligible sample set from the input margin and rejects a trial whose pre-clip prediction margin is non-positive. The interior penalty is the §3.9 barrier: mirrored logarithmic walls on the reference tone and output lightness plus the neutral-anchored Bregman barrier on both fitting inverse paths. Rendering contracts an out-of-domain input toward the D65 neutral at the same luminance so evaluation stays finite. There is no "any RGB channel ≤ 0" rule.

Strict feasibility is independent of the penalty weights. The §3.9 barrier is a smooth interior penalty, not a blow-up guarantee at every boundary: a zero weight can overpower logarithmic growth, so `evaluate` returns a typed violation instead of a non-finite value. §3.9 states the regularity limits: at an outer output-neutral crossing with active clipping the post-clip UCS response can contain `|z|z`, its gradient is continuous and locally Lipschitz but its classical Hessian is undefined, and the fitter uses the composite gradient with a positive Gauss-Newton model for those residual rows.

### 3.1 Tone `T`
`J'_base = T(s_L)`. Monotone non-decreasing spline (I-spline), ~20 knots over [−12, +6], dense in [−4, +3], including the shoulder. Outside the knot range the I-spline continues by integrating the end cubic piece, which is C³ at the join. The colorimetric tone is `T(s) = J'` of the neutral at physical `s`, clamped to the tone range.

### 3.2 Neutral tint `N`
`(a'_N, b'_N) = N(J'_base)`, two smooth splines (~12 knots) over `J' ∈ [0, 100]`. Above 100 the tint continues C²: it matches value, slope and second derivative at 100 and reaches zero slope and curvature at `J'_max = solve::neutral_j_prime_max()`, the neutral inverse limit, then stays constant. The continuation is linear in the tint coefficients, so the fit Jacobian uses the same weights. With D = 1, a neutral input gives `M'_ref = 0` (§3.3), so N measures neutral tint exactly.

### 3.3 Chroma, hue and lightness relative to a colorimetric reference
To separate tone from color, G, R and Λ are defined **relative to what a purely colorimetric rendering would produce at the neutral-matched luminance**:

1. Find `Y_ref`, the luminance of the neutral whose CAM16-UCS lightness equals `T(s_L)` (closed form). `XYZ_ref` is the input chromaticity at luminance `Y_ref`.
2. Two pre-clip chroma paths are evaluated at `XYZ_ref`. The **UCS path** is the colorimetric CAM16-UCS chroma `U` with the layers applied in UCS; the **internal path** is the normalized opponent chroma `N` of `XYZ_ref` with the same layers applied in opponent coordinates. `J'` is shared. The join weight is the input's `w(|n|)` from §3.3.5.
3. Layers over `(h_in, s_L, √C_in)`:
   - chroma gain `log G`;
   - hue rotation `R` (degrees);
   - lightness shift `Λ` (J′ units).
4. Output:
   ```
   J'           = J'_ref + Λ(h_in, s_L, C_in)
   (a'_U, b'_U) = N(J'_base) + G·rot(R)·U + residual
   (n_a, n_b)   = N(J'_base) + G·rot(R)·N + residual
   (a', b')     = w·(a'_U, b'_U) + (1 - w)·Φ(n_a, n_b, J')
   ```
   `Φ` is the exact CAM16-UCS chroma of the stimulus the internal coordinates reconstruct (`Cam16Model::ucs_chroma_from_opponent`): `t = P·e_t(h)·|n|/K`, `C = t^0.9·√(J/100)·t_a`, `M' = ln(1 + c2·F_L^0.25·C)/c2`. `Φ(N_ref, J'_ref) = U`, so the identity Look is colorimetric on every probe, including exact neutral, where `Φ(0) = 0`. `w = 1` for `|n| ≥ C0`, so outside the join the UCS path is published unchanged. The residual is looked up at `(s_L, a_r, b_r)` with `(a_r, b_r) = w·(a_in, b_in) + (1 - w)·(n_a, n_b)`, the same blend applied to the input at `Y = 0.18`.

5. The fitting residual is formed in the same two systems. For one sample with observed UCS `O`, predicted post-clip UCS `P` and post-clip internal chroma `M`:
   ```
   r = w·(P - O) + (1 - w)·(M - Ψ(O)),     Ψ = Φ⁻¹
   ```
   `Ψ(O)` is computed once per sample from the observation and stored. The lightness row is the same blend. Published predictions, comparisons, the 1.5 ΔE support threshold, release ceilings and card evaluation use `P` in CAM16-UCS.


T is the neutral-axis tone curve. Chromatic colors keep their colorimetric lightness relative to the neutral of the same luminance.

With colorimetric `T` (`T(s)` = neutral `J'` at `Y = 0.18·2^s`), `Y_ref = Y` and, for `|N| ≥ C0`, the output equals colorimetric CAM16-UCS of the input.

**G = 1, R = 0, Λ = 0 means "tone change only, otherwise colorimetric".** Two looks that differ only in tone report identical G/R/Λ.

Splines are tensor-product B-splines: **periodic cubic in hue (24 knots)** × cubic in `s_L` × cubic in √C (4 knots). The **s-knots (10) are a subset of the tone knot vector**: dense where T is dense, including the shoulder, so highlight desaturation is representable. At `C_in = 0` the hue axis has no meaning, so the `C = 0` coefficient plane is shared across hue and G/R/Λ are hue-independent there.

The outer representation `O(h, s, C) = c0·P(s) + Σ_{c=1..3} c_c·V_c(h, s)` is C^∞ for `C > 0`, but its Cartesian `(a, b)` derivative grows like `C^(-1/2)` at the neutral origin because `c1 ≈ 3√(C/C_MAX)`. A near-neutral **Cartesian core** replaces that singular behaviour:

```
K(n_a, n_b, s_L) = Σ_s s_w(s_L) Σ_t κ[s,t] B_t(n_a/C0, n_b/C0)
B = [n_a/C0, n_b/C0, (n_a/C0)², (n_a/C0)(n_b/C0), (n_b/C0)²]
C0 = 2,  (n_a, n_b) = the normalized opponent chroma at Y = 0.18
F = w(|n|)·O + (1 - w(|n|))·(P + K)
|n|² = n_a² + n_b²
w(|n|) = 1 - (1 - |n|²/C0²)³   for |n| < C0
w(|n|) = 1 for |n| ≥ C0
```

`w` is a polynomial in `|n|²`, hence C^∞ in `(n_a, n_b)`. At the neutral end `w(0) = w'(0) = 0` and `w''(0) = 6/C0²`; at the join `w(1) = 1` and `w'(1) = w''(1) = 0`. So `w = O(|n|²)` at neutral and `1 - w = O((C0 - |n|)³)` at the join, with `max|w''| = 1.5`. Since `O - P = O(|n|^(9/20))` (the outer chroma term goes as `√C_in` and `C_in` as `|n|^(9/10)`), `w·(O - P) = O(|n|^(49/20))`; its first derivative is `O(|n|^(29/20))` and its second is `O(|n|^(9/20))`, both continuous and zero at the origin. `(1 - w)(P + K)` is C^∞ in `(n_a, n_b)` and C² in `s_L`. Thus `F` has continuous first derivatives and bounded continuous second derivatives across neutral and across the join, and is C² at the join. Hue harmonics of order `k ≥ 3` enter only through the outer term and are suppressed by `w = O(|n|²)`; the core itself is a quadratic polynomial with harmonics 0, 1 and 2. The normalized coordinates themselves are a scalar multiple of the identity at the origin, so no angular factor enters: the kink left by undoing only the `0.9` chroma power is the eccentricity factor `e_t(h)`, whose one-sided slopes are `e_t(0) = 0.8460` and `e_t(180°) = 1.0540`, and it is cancelled exactly.

At `|n| = 0`, `K = 0` and `F = P`; at `|n| ≥ C0`, `F = O` exactly. The `|n| = 0` plane is still shared across hue, the neutral output is still `J' = J'_ref + P_Λ`, `a' = N_a`, `b' = N_b`, and a tone-only Look (`P = V = κ = 0`) still reports `G = 1`, `R = 0`, `Λ = 0`. For `|n| ≥ C0` the 24-knot hue spline is untouched. The core adds 5 coefficients per `s` per component (50 per component; `GRL_COEFS` 780). `C0` and the weight live in `basis::NEUTRAL_JOIN_C` and `basis::outer_weight`.

Volume-only control-response features are attenuated by `w` below the join; plane features are not. With `C0 = 2` and the polynomial weight:

| `C` | `w` | volume-feature attenuation |
|---|---|---|
| 0.2 | 0.0297 | 3.0% |
| 0.5 | 0.1760 | 17.6% |
| 1.0 | 0.5781 | 57.8% |
| 2.0 | 1.0 | 100% |

### 3.4 Output encoding and soft clip
The Look predicts appearance before output encoding. To compare with JPEGs: J′a′b′ → XYZ → linear output RGB (the JPEG's primaries) → per-channel **soft clip** → back to XYZ → CAM16-UCS J′a′b′, where residuals are measured. This post-clip frame is the rendered-error frame of §5; the target uses its pinned reference encoding and clip, while a camera prediction uses the selected output encoding and that realization's clip. (The JPEG side is decoded through its TRC to linear first, so no TRC round-trip is needed on the prediction side.)

```
c(x) = x                                   for k ≤ x ≤ 1−k
c(x) = (1−k) + k·tanh((x − (1−k))/k)       for x > 1−k
c(x) = k − k·tanh((k − x)/k)               for x < k
```

The function is C¹ and maps (−∞, ∞) → (0, 1). `k` is a per-realization parameter (default 0.03, fitted per `(body, epoch, sim)`; prior N(0.03, 0.02²) truncated to [0.005, 0.1]). The Look file stores the function id and each contributing realization's `k` in its provenance; the coefficient artifact carries the reference encoding and nominal `k`, not a camera-specific fitted `k`. The published Look itself is pre-clip.

Composite fitting selects absolute XYZ when its infinity norm is smaller than the white-relative departure's, and white-relative XYZ otherwise. For a reference-white-preserving output space, the latter preserves the soft clip's upper-tail deficits before adding white. The forward CAM16 anchor is exactly achromatic under complete illuminant discounting. The selected chart supplies the value and first Jacobian; curvature uses the same chart. Rounding clipped RGB to white does not erase a finite chromatic pullback. Scalar checkpoints use generation 6 and hierarchical checkpoints use generation 3 for this evaluation, fixed per-image retained-row selection, stage-specific image weights, family-scoped firmware offsets and the free/fixed body-coordinate layout. Serialized Look coefficients use absolute coordinates.

First, second, clip-width and mixed soft-clip derivatives share a cancellation-free exponential-tail evaluation.

Pre-clip values in saturated regions are only weakly constrained by data, because the clip's derivative → 0. Through the prior curvature floor (§5) this appears as a large pre-clip σ, while the post-clip value stays supported and gated. The clip derivative at the probe, not `x < k`, is the quantity that decides whether the pre-clip value is identifiable.

### 3.5 Residual LUT
A 17³ grid of control points over `(s_L, r_a, r_b)` outputs ΔJ′a′b′ through a uniform cubic B-spline, C² across cell faces. The residual coordinates are the blended `(r_a, r_b) = w·(a_in, b_in) + (1 − w)·(n_a, n_b)`, and the s axis extrapolates its end cubic piece outside the knot range. Regularization: Laplacian smoothness + L2. Control points with negligible density are pinned to 0. A large residual is itself a diagnostic.

### 3.6 Coverage / density
A kernel density estimate of the training samples in `(s, h_in, √C_in)`, weighted by trust weights. Used for support (§5) and bounties.

### 3.7 Look metadata
`wb_policy`:
- `adaptive` (v1): applies to WB'd colors.
- `fixed_illuminant`: later.

Also: viewing conditions, probe set version, kind, provenance (including input-transform, control-response-model and anchor versions; [03 §3](03-data-model.md)). The Look document is the shared family target. Camera calibration, realization parameters, CRM and clip are separate versioned records referenced from provenance, never serialized as target coefficients ([03 §1](03-data-model.md)).

### 3.8 LUT-defined Looks

A `lut_defined` Look's target function is the composition `scene XYZ → declared encoding → LUT → declared output encoding → CAM16-UCS appearance`. Its identity is the LUT bytes, the declared encoding and the base; the layered approximation is fitted to that composition and frozen, so evaluation, previews, cards and matching never read the base at runtime. Its qualified-matching eligibility is the existing import, frozen approximation and base-pin validity with inherited support and uncertainty, plus current base applicability; no native-camera admission or native-prediction report on the LUT is required. Native-family cohort admission does not train or refit a `lut_defined` target from camera uploads: its LUT bytes, declared encoding, base pin, frozen approximation and license semantics are unchanged. An existing base publish, base resolution or root revocation still triggers the documented rebase of a dependent LUT.

**Scene-referred log.** The encoding is a transfer and a gamut. v1 implements F-Log2 and F-Gamut from Fujifilm's [F-Log2 Data Sheet Ver.1.1](https://dl.fujifilm-x.com/technical-data/F-Log2_DataSheet_E_Ver.1.1.pdf) §2-3: with `a = 5.555556`, `b = 0.064829`, `c = 0.245281`, `d = 0.384316`, `e = 8.799461`, `f = 0.092864`, `cut1 = 0.000889` and `cut2 = 0.100686685370811`, encode a reflection `r` as `c·log10(a·r + b) + d` for `r ≥ cut1` and `e·r + f` otherwise, and decode a code `v` as `(10^((v−d)/c) − b)/a` for `v ≥ cut2` and `(v−f)/e` otherwise. F-Gamut uses the data sheet's primaries (R 0.708, 0.292; G 0.170, 0.797; B 0.131, 0.046; D65), so its RGB↔XYZ matrices are the normalised-primary-matrix solution for those primaries. The `.cube` domain `[0, 1]` is the code range; values outside it clamp as the LUT reader does.

**Display-referred.** The LUT expects an already-rendered image, so the definition pins a base rendering: a published Look version `look@version`, or a body realization when a shared realization parameter fails ([10 Q18](10-open-questions.md)). The fixed baseline `looklab-neutral-srgb-v1@1.0.0` is never refit and contributes zero base uncertainty. Its rendering is a published standard, plain ITU-R BT.709 OETF after a hard clip: with `s = clamp(x, 0, 1)`, output `4.5·s` for `s < 0.018` and `1.099·s^0.45 − 0.099` otherwise, applied per channel to scene-linear Rec.709 RGB. The choice between this baseline and an ACES SDR output transform is open ([10 Q25](10-open-questions.md)).

**Uncertainty.** Every import stores the fitted approximation's per-probe covariance, its post-clip σ, support, density and pre-clip identification. A pinned measured base's covariance is composed on top: `Σ_enc(p) = J_enc(p) Σ_base(p) J_enc(p)ᵀ` and `Σ_total(p) = J_LUT(p) Σ_enc(p) J_LUT(p)ᵀ + Σ_fit(p)`, with `Σ_base` the base's post-clip appearance covariance, `J_enc` the base's appearance-to-encoded-RGB Jacobian, and `J_LUT` the Jacobian of the composed appearance with respect to the base's encoded RGB. The stored support is the intersection of the fit's and base's support, the stored pre-clip identification requires both the fit's and base's clip state, and the base's density replaces the fit's where present. Probes whose composed σ exceeds the normal support threshold, or whose fit or base covariance is missing or non-finite, become unsupported. Where the base itself clips, the composition is defined post-clip only, and those probes carry the base's clip state rather than an identified pre-clip value. The fixed neutral baseline contributes zero base uncertainty; the fit's approximation uncertainty still applies.

**Base status.** A display-referred definition is `pinned` when its base is published and fitted. `base_identified_unmeasured` means the base is fully declared (body, renderer, film simulation, dynamic range) and consistent with the body file but not yet measured or published. `base_incomplete` means a declared field is missing. `base_inconsistent` means the declared setting does not exist on the declared body and names that field. None of the last three is matchable or gets a card file. A source-only version may publish the original LUT composed with the best identified approximation of its declared base: the latest matchable version of the declared Look, or a published native rendering whose family labels match the declaration. The descriptor records that composition base; when no identified rendering exists it records the fixed neutral baseline as a fallback. The approximation is never presented as the declared base, camera controls or firmware. A better approximation or the exact declared base publishes a new immutable version; the ordinary URL follows the current selection. When a valid fitted representation is computed against an identified approximation, the document's encoding names that computation base, a typed `declared_base` records the original, and the version is published with an experimental approximation marker; the declared base's camera controls and firmware are not claimed. The first fitted representation is checked for convergence, the existing 1.0 ΔE supported-probe p95 accuracy requirement and supported probes, not compared against itself, and does not require a moderator re-anchor merely because earlier versions were previews.

### 3.9 Domain penalty

For fitting inverse path `p ∈ {internal, UCS}` and cone channel `c`, let `R_a[p,c]` be the pre-clip adapted response reconstructed by the inverse model and `q[p,c] = (R_a[p,c] − 0.1)/400`. The anchor `q0(j)` is the neutral response at the shared output `J' = j`. With `φ(q) = −log(1 − q²)`,

```
D(q, q0) = φ(q) − φ(q0) − φ'(q0)(q − q0)
F[p]      = Σ_c D(q[p,c], q0(j))
B         = b(T) + b(j) + (1 − w)·F[internal] + w·F[UCS]
b(x)      = −log(1 − x/j_max) − log(x/j_max)
```

where `T` is the tone consumed by the neutral-reference inverse, `j = J'_ref + Λ + residual[0]`, `w` is the input-neutral blend weight, and `j_max` is the inverse-lightness limit derived from the actual inverse and viewing conditions. `B` carries the fixed barrier strength and the existing sample weight. The two `b` terms are the mirrored pair of logarithmic barriers with the same strength: `−log(1 − x/j_max)` holds the upper wall, `−log(x/j_max)` the lower wall, with gradient `−1/x` and curvature `1/x²` from the lower term. `b` is minimized at `x = j_max/2` with value `2 log 2`. Its gradient `1/(j_max − x) − 1/x` is negative for every `x < j_max/2`, which includes the whole rendered `J'` range: the barrier raises both `T` and `j` everywhere below the midpoint, not only in the shadows. For `j_max = 240.85` the gradient at `J' = 100` is about `−0.0029` per unit sample weight. Serialized coefficient meanings are unchanged; the objective and checkpoint identity change.

`DomainAxis` orders the seven differentiated coordinates `Tone, Lightness, UcsA, UcsB, InternalA, InternalB, Blend`. `evaluate` returns the value, gradient, exact Hessian and a positive Gauss-Newton part in the same axis order, plus a diagnostic `minimum_margin`. The minimum is never differentiated. `q0(j)`, both path responses and `w` are differentiated, including lightness, white-balance, tint and residual chain terms. The internal responses are reconstructed directly in Cartesian opponent coordinates, not through UCS fractional chroma. The published rendering inverse is validated separately on the appearance-before-inversion blend; it is never differentiated and never uses the rendering contraction fallback.

Strict feasibility is independent of the penalty weights: `0 < T < j_max`, `0 < j < j_max`, `|R_a[p,c] − 0.1| < 400`, a positive finite temporary-magnitude denominator for both paths, and a positive finite Cartesian reconstruction denominator for the internal path. The UCS inverse's branch denominator is branch data, not a feasibility margin. A zero weight does not relax feasibility. `evaluate` returns a typed `DomainViolation` instead of a non-finite value. Exact black is a rendering boundary, not an ordinary Hessian point. `RenderableAppearance::NeutralBlack` carries no `PublishedInverse` certificate; derivative-based export paths require the regular interior inverse.

Regularity: at zero output UCS chroma the response departure is `O(|z|^(10/9))` and the anchored penalty is `O(|z|^(20/9))`; its chromatic Hessian tends continuously to zero, jointly with lightness when the anchor moves with `j`. At zero input opponent chroma the UCS reference varies as `O(|n|^(9/10))` while `w = O(|n|²)`; `w'(0) = 0` but `w''(0) ≠ 0`. The chained chromatic Hessian therefore contains `Hessian(F_internal) + (F_UCS − F_internal)·Hessian(w)`; the outer penalty value at `w = 0` and the internal value at `w = 1` are preserved.

The Bregman remainder is evaluated without cancellation: with `delta = q − q0`, `D = R(−delta/(1 − q0)) + R(delta/(1 + q0))` and `R(x) = x − log1p(x)`, using the series for small `|x|`. The lower wall is evaluated as `−log(x/j_max)`, a single division and logarithm, so the blow-up as `x → 0` keeps its full relative accuracy.

At an outer output-neutral crossing with active clipping the post-clip UCS response can contain `|z|z`. Its gradient is continuous and locally Lipschitz, but its classical Hessian is undefined at the crossing. The fitter uses the correct composite gradient and a positive Gauss-Newton model for the affected residual rows; it does not label that model an exact Hessian and does not turn arbitrary non-finite failures into Gauss-Newton fallbacks. Exact-Hessian checks stay in smooth regions, with one-sided regularity checks at crossings.

## 4. Fitting

### 4.1 Objective and noise model
Per-sample negative log-likelihood (multivariate Student-t, ν = 4, 3-D residual r):

```
ℓ_ip = ((ν+3)/2) · log(1 + ||r_ip||² / (ν σ_v²)) + 3 log σ_v
```

Inside the `C0` transition the residual LUT and its Jacobian use the near-neutral Cartesian coordinates of §3.3, blended with the UCS coordinates by `w`; outside they use CAM16-UCS. Published predictions, the 1.5 ΔE support threshold and the release ceilings remain CAM16-UCS.

The total objective is:

```
Σ_i  (n_eff / n_i) · Σ_p  ω_ip · ℓ_ip
  + B (domain penalty, §3.9)
  + smoothness/shrinkage terms (λ_T, λ_N, λ_GRΛ, λ_res)
  + priors (x_i, w_i, ξ, ω, Δ_c, k, firmware offsets, σ_v)
  + chart term
```

`B` carries the fixed barrier strength and the sample weight; it is not Student-t reweighted and adds no GCV parameter.

- **σ_v appears only here**, including its log term, so inflating it isn't free. It's **not** repeated as a weight factor ([05 §5](05-trust-and-integrity.md)). Prior: inverse-gamma on σ_v², centered at 1.5 ΔE.
- **Tempering:** samples within an image are strongly correlated. The factor `n_eff/n_i` (default n_eff = 500) keeps one image from overwhelming the priors and nuisance terms. Uncertainty is still computed with the sandwich estimator (§5).
- `ω_ip` = density weight × the image's trust weight from 05; the stored `density_w` is the raw inverse inclusion probability, normalized to mean 1 within each image so it reweights samples without changing the image's total weight.
- Fitting uses up to 10k samples per image by default (a uniform draw within the stored ≤ 50k set).

**Staging** (tone first, after dtsolve's practice). Each stage warm-starts the next:
1. T, x_i (and ξ) on near-neutral samples (`C_in` small; well-defined because D = 1).
2. Add N.
3. Add G, R, Λ.
4. Fit the residual on the residuals, then do a short joint refinement.

**Regularization:** choose each λ **within its own stage** by generalized cross-validation (GCV) on that stage's linearized problem, using a 1-D search over ~8 log-spaced values. For a candidate λ, GCV solves `(H + C + λP)·δ = −g − g_fixed − λPθ̂`, where `H`, `g` and the RSS are data over the same sampled linearization rows and are scaled by active samples per Hessian row. The block step solve, including the matrix-free residual grid, uses that scaled Hessian with the exact full-data gradient, so the row cap changes the quadratic model, not the stationary point. `C` and `g_fixed` are the fixed prior curvatures and gradients (exposure, WB, mode, clip and identity priors), and `P` is the smoothness curvature. The fitted RSS uses `H` and `g`; the degrees of freedom are `tr(H·(H + C + λP)⁻¹)`. With the fixed priors in the system, the exposure/mode gauge direction is regularized rather than left singular. The Student-t rank-one correction can make `H` indefinite, and then which candidate `λ` admit a factorization is an artifact of the factorization rather than of the data; when `H + C` is not positive definite the search uses the diagonal approximation of the same criterion. K-fold CV over images is a periodic quality check on a per-image subsample, not part of every fit. Store the chosen λ in the Look file. The response block's λ carries a floor of 1.0: the frozen response pass at a floor of 100 over-smoothed `D` and raised held-out re-render error from 1.15 to 2.21 ΔE on the controlled fixture.

**Identity prior:** shrinkage toward the identity Look is a fixed Gaussian prior on each layer, not a smoothness term. For every other identity-prior layer the energy is `0.5·θ²/sd²` and the curvature `1/sd²`, with tint 2 J′a′b′, `log G` 0.3, hue rotation 15°, lightness 8 J′, residual 0.5 ΔE. The tone layer is centered on the fixed representable colorimetric initializer `m_T`, whose fitted coordinates are `c0 = c0_colorimetric` and `η_k = softplus⁻¹(β_k)` for the colorimetric spline coefficients, using the same `β = max(β, 1e-6)` inverse-softplus convention as initialization. Its energy is `0.5·(θ_T − m_T)ᵀ·W_T·(θ_T − m_T)` and its curvature is the unchanged precision `W_T = 1` on every tone coordinate. This is a Gaussian in the fitted `(c0, η)` coordinates: `c0` is in J′ and each `η_k` is the unconstrained coordinate of the I-spline coefficient `β_k = softplus(η_k)`, so the precision acts on `η`, not on the physical `dT/ds` slope; it is not an exact Gaussian in J′ per stop. The center is computed once per layout and reused by the initializer, the objective and every gradient. Core coefficients use their layer's SD; the core basis functions are O(1) on `|a|, |b| ≤ C0`. These weights do not weaken as the pool grows. Their curvature enters `H`, the covariance floor and the prior-bias test (§5). Serialized coefficients remain absolute (`c0` and `β_k`, not offsets); only the objective and the `FitStateGeneration` checkpoint identity change.

Solver: Levenberg–Marquardt with dense Cholesky on each block and preconditioned conjugate gradients on the matrix-free residual grid. A candidate step is judged by a value-only trial at the current σ against the quadratic predicted reduction; damping follows the gain ratio and σ is re-optimized only from the accepted trial. The final joint refinement runs two step models, each with its own trust region. The analytic model uses the Student-t corrected curvature operator and caps each metric proposal at `|δ|∞ ≤ 2`; at every successful proposal the caller realizes the capped proposal and the feasible Euclidean Cauchy step `-αg` in the same unconstrained joint θ coordinates, compares their applied predicted decreases under that operator, and takes the larger (the proposal on ties). A coordinate-clamped box alternative is evaluated together with the ordinary winner only when its predicted decrease ranks strictly first; the strictly larger realized decrease is applied and ties keep the ordinary winner. The positive model uses the undamped joint IRLS/Gauss-Newton curvature `B` with the same diagonal metric, the same coordinate cap and the same preconditioner, and no damping. Each model keeps its own radius, adapted only from its own candidate's realized gain ratio. Both candidates are evaluated on the true objective, every evaluation is charged to `evaluations` and `sample_evaluations`, and the feasible candidate with the lower realized objective is applied; ties keep the analytic candidate. A failed subproblem contributes no candidate and shrinks only its own radius. The per-attempt Hessian-product allowance is split between the two solves, not increased. The analytic benchmark remains feasible Euclidean Cauchy decrease in solver coordinates, not an exact-Hessian model. A nuisance block whose image rows form independent per-image groups is solved by Schur elimination of those groups, leaving one dense solve over the shared mode offsets and clip; blocks without that structure fall back to dense Cholesky.

**Near-neutral and boundary derivatives.** The G/R/Λ Jacobian columns are Cartesian in `(n_a, n_b)`: `∂F/∂n_a` and `∂F/∂n_b` combine the core's polynomial gradient with `(∂w/∂n)·(O − P − K)`, and the UCS columns `∂F/∂a`, `∂F/∂b` (the outer's `∂/∂h` and `∂/∂C`) are multiplied by `w`. At `|n| = 0` the code uses the core gradient and the weight gradient and does not evaluate `1/|n|` or `1/C_in`; no arbitrary zero derivative is substituted. The fit differentiates the §3.3.5 residual, not the published output: each path contributes its own `d residual/dθ` and the two are joined by `w`, with the `(∂w/∂θ)·(r_ucs − r_int)` term on the nuisance columns. The output clip path uses `clip::preclip_xyz_jacobian` for the UCS path and `clip::preclip_internal_xyz_jacobian` for the internal path; the latter is the inverse of the analytic forward Jacobian of `(n_a, n_b, J')` and is linear at the neutral origin, so no chroma power is differentiated. The published-output Jacobian (`Look::d_output_d_layer_values`) chains `Φ`; at a neutral internal chroma `Φ` has no finite Jacobian, so the model substitutes the internal derivative and `ChromaPaths::singular()` marks the location (see §5). The residual is a cubic B-spline in `(s_L, a_r, b_r)` with the same blend. Every spline layer's s-slope is multiplied by `E'(s)`; `E'` is 0 outside the lookup domain, 1 in the identity interior, and continuous at the joins. The tint continuation is C² at `J' = 100`. The clip Jacobian is `∂post/∂pre = J₁·T·diag(c')·F·∂XYZ/∂pre`; its chromatic columns vanish at a neutral pre-clip prediction, so no inverse is formed and no identity is substituted. The barrier margin is evaluated on the UCS pre-clip appearance and its exact `d margin/dθ` is returned per column (`EvalOutput::margin_terms`) for hard rejection; it is not the differentiated objective. The interior penalty is the §3.9 barrier: mirrored logarithmic walls on the reference tone and output lightness plus the neutral-anchored Bregman barrier on both fitting inverse paths. The remaining domain is handled by explicit predicates on the failing quantity (nonpositive luminance, an adapted-cone response at its singularity, the inverse limit derived from the actual inverse and viewing conditions). A singular point is either smooth through the normalized coordinates and continuations or a domain failure; there is no identity-inverse fallback.

### 4.2 Joint Look + control-response fit
**Reference setting:** every control marked `look_affecting: true` in the body's schema is at its declared `default`. This is the single definition of "target-defining" ([03 §1](03-data-model.md)). Spatial controls (grain, clarity, sharpening, NR) must be at default or at a schema-listed tolerated value for an image to be used at all.

Eligible native images from the explicit seed/reference or admitted cohort are used, **at any settings**. θ (the family coefficients the body's sim realizes) and the cohort's control-response coefficients D ([04 §2](04-matching-and-export.md)) are fitted **jointly**:

```
θ(k, u) = θ_L + δ_{fw} + Σ_j φ_j(u_j) D_{c,k,j} + interactions
```

So non-default library images from cohort bodies inform θ through the shared θ_L, rather than being thrown away. A body outside the cohort runs the conditional realization fit: `θ_L` stays at the pinned family version, and only that subject's D, realization parameters and body calibration versions update. The cohort is explicit; there is no unconditional all-body pool followed by an exception.

Inputs are partitioned by the actual resolved `(body, epoch, native_sim)` realization subject before schema or reference-setting selection. Two epochs on one body, or two sims mapped to one family, are separate subjects with separate CRM, clip, support and fit attempts; no body-wide flag stands in for that scope. Missing control values are unknown, not defaults. A family-target fit estimates `θ_L` and D jointly over the explicit seed/reference or admitted cohort only. A realization fit against a pinned family version holds `θ_L` fixed and runs the body and realization block for its subject: shared body calibration `Δ_c`, `ξ` and `ω` may update through new body-level versions, while D and clip update only that subject. It publishes a realization revision that pins the resulting versions. Experimental realization fits run that conditional stage and cannot write qualified coefficients, gauges, support, warm starts or weights ([05 §8.2](05-trust-and-integrity.md#82-experimental-isolation)).

### 4.3 Global hierarchical fit (multi-body)
The unknowns:
- per Look family: θ_L;
- per body: Δ_c, ξ_{c,m}, ω_c; per realization `(body, epoch, sim)`: D_{c,k}, clip k_{c,k};
- per firmware version: small offsets δ_fw on T and G coefficients (tight priors);
- per contributor: σ_v;
- per image: nuisances.

Each refit runs in **two stages**, each minimizing a single well-defined objective:

1. **Native stage.** Use native data only (all settings) plus chart terms. Block coordinate descent to convergence:
   - (a) per-body and per-realization parameters (Δ_c, ξ, ω, **D**, clip k) given all θ_L;
   - (b) per-family θ_L (and δ_fw) given body and realization parameters.

   D appears in block (a) only. A realization-only refit holds the pinned θ_L fixed and runs block (a) for its subject, warm-started from that subject's previous realization state; it publishes a realization revision, not a `LookVersion`.
2. **Recipe stage.** Hold every θ_L fixed. Refit the per-body and per-realization parameters (Δ_c, ξ, ω, D, clip k) on native + recipe data, warm-started from stage 1. These are the published body and realization parameters.

Both blocks minimize the total objective of §4.1, including the sample-weighted domain penalty `B`; the body block carries its value, differentiated body-coordinate gradient and Gauss-Newton curvature.

A family that lacks a target may be seeded once from eligible native evidence on any supported body under existing trust, resource and persisted-gauge policy. Seeding creates one provisional canonical family version, records its source limitations, grants no `RootAdmission`, and is fenced so concurrent jobs cannot select competing heads. Thereafter a realization fits against the then-current pinned target: seed-cohort evidence may refit that target, while a body outside the cohort runs only the conditional stage and cannot move it. A family candidate is immutable coefficients plus dependencies; a realization revision pins the exact `look@version` and changes without redefining the target. Maximum version is not a family selector: a higher-version camera-local diagnostic fit is never a target, previous family release or anchor. A new family version queues dependent realization work against that candidate; existing realization revisions stay pinned to their original family version. A camera-only realization update publishes a new `MatchBundleRevision` ([03 §1](03-data-model.md)); it does not create or rewrite a target release.

A family's seed or reference cohort is the set of bodies whose eligible evidence established its canonical target. Seed membership is source-limited and does not itself grant cross-body qualification; the seed cohort may refit the same target as eligible data arrives under unchanged gates, so a source-limited target does not freeze until a second body supplies P1.9 evidence. The cross-body-qualified cohort is separate and expands only through scoped admission, which requires the P1.9 agreement and the applicable engine prerequisite (§5.1). A new body's cross-body membership remains experimental: it fits its realization against the pinned family version and cannot silently join the seed cohort. Neither an update from an existing seed body nor a later admission resets the family anchor, gated union, support history or gauge identity; admission schedules a new family candidate and its gates ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation), [03 §1](03-data-model.md)). Native and recipe stages remain distinct; admission never relabels native evidence as recipe.

The hierarchy preserves per-image illuminant handling: every image is evaluated through `A_c(T_i)` at its server-recomputed CCT. `Δ_c` is shared body calibration across that body's realizations; `ξ` and `ω` are body-level; CRM `D` and clip `k` are keyed by `(body, epoch, native_sim)` and the pinned family version. A single per-body CRM or clip scalar, or a D65-only transform use, is not the production realization scope. Experimental evidence is filtered before graph construction, loss construction, shared weighting and contributor-noise estimation, and cannot influence qualified coefficients, calibration, gauges, support, warm starts, shared contributor noise, scientific reputation or consistency state, weights or graph state through indirect paths even when its direct Look weight is zero ([05 §8.2](05-trust-and-integrity.md#82-experimental-isolation), [model qualification plan §6](model-qualification-plan.md#6-isolation-before-computation)).

**Recipe images contribute nothing to any θ_L**, not just to the Look they attempt. Splitting into two stages (rather than letting recipe data into block (a) of a single alternation) keeps each stage a true minimization, so it converges instead of oscillating between two objectives. Card renders (intent `card`, [07 §4](07-contribution.md)) enter neither stage.

**Warm starts:** each native stage starts from the **previous native-stage solution**, never from the published recipe-stage parameters. Otherwise recipe data could still influence which local optimum the Looks land in. A realization stage also starts from that subject's own previous realization state, not from another body, epoch or sim.

**Refit cascade:**
- An update to a body (new chart session, new data) marks the body and its affected realization subjects dirty.
- A nightly batch runs the native stage (blocks (a) and (b) for dirty bodies and every Look native on them), then the recipe stage for those bodies, then weighting and release gates ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)). A family-target refit runs when the seed/reference cohort or an applicable dependency changes; otherwise the batch runs only the conditional realization stage against the pinned target.
- Look provenance records, for every contributing body, the input-transform version, **the native-stage body parameters the Look was fitted against**, and the realization revision ids and CRM versions. A realization revision records its own exact family version, transform, parameters, CRM and clip. Those dependencies are never published as a body version otherwise.

### 4.4 Outlier scoring (per image)
For each image: weighted robust RMS ΔE against the prediction, then a robust z-score within the applicable subject and pool `(Look, body, epoch)` using median/MAD. The pool is scoped to the evidence entering that fit, not a global pool.

Consistency jobs use the scoring-only fit path. Probe covariance, control-response covariance and K-fold model-error refits run on the publication path. A diagnostic run against an experimental realization may expose the same scores, but it must not write authoritative consistency, reputation, weight or quarantine state used by qualified fitting; intake validation and human trust decisions remain separate ([05 §8.2](05-trust-and-integrity.md#82-experimental-isolation)). The thresholds below are unchanged.

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

The projection uses the fitted shared-family Look, one body realization's authoritative-stage parameters (native for a free fit, the pin's stage for a conditional fit) and only its eligible native evidence with a positive native Look weight; a positive result opens a moderation proposal only after that scoped fit commits, with insertion fenced by the job lease, numerical sources and accepted evidence snapshot including the exact accepted family target.

## 5. Uncertainty

Per Look version, each probe stores a 3×3 covariance for the **mean Look response**, including systematic model discrepancy. Matching, comparison, card translation and release gating use this uncertainty. Observation noise affects how precisely the mean can be estimated; its full per-JPEG variance is not added to every published probe. Future-JPEG predictive uncertainty is a separate quantity, not the stored `σ_ΔE`.

**Rendered-error frame.** Matching, verification, recipe scoring and reporting compare appearances after each side's own output encoding and clip. For each retained sample, fix one calibrated scene input `p` independent of the candidate recipe:

- **Target `t`:** the canonical target at `p`, rendered through the target version's pinned reference encoding and clip.
- **Prediction `m`:** the pinned camera realization at `p`, with the recipe's settings, actual output encoding and realization clip.
- **Observation `o`:** the captured JPEG decoded through its declared encoding into reference CAM16-UCS.

All three use the same viewing conditions. A decoded JPEG sample is never inverse-clipped or soft-clipped a second time. Renders in different output encodings are compared in the common UCS frame; their different gamut limits stay part of the result. Exposure compensation and WB adjustments move the camera prediction, not the target; they cannot change sample membership or re-bin colours, and body exposure/WB offsets remain realization parameters. A capture-specific nuisance correction is frozen before scoring, never refitted per recipe.

The descriptive errors are `e_predicted = ‖m − t‖`, `e_captured = ‖o − t‖` and `g_raw = e_captured − e_predicted`; the signed vector `o − m` is retained separately because the difference of distances loses direction. A rendered error is identified by `ScoringMetric::RenderedPostClipV1` (`looklab-scoring-metric/rendered-post-clip/1`) and its `OutputContext { target: OutputPin, camera: OutputPin }` ([03 §1](03-data-model.md#rendered-metric-output-context-and-assessment-status)); values from different identities are not comparable. A historical pre-clip score uses `LegacyPreClipV1` with no output context and keeps that meaning. The target enters as `TargetAppearance::Rendered(OutputPin)`; `TargetAppearance::PreClip` is the historical diagnostic form and never the matching or bounty objective.

**Systematic excess.** A positive raw gap is not by itself model error: even a correct mean-response model has positive expected JPEG distance from observation noise, and averaging captures does not remove that noise floor from the mean of distances. Raw captured ΔE and `g_raw` are descriptive scores. The persistent-residual estimand is the **systematic conditional-mean excess**

`g_systematic(p,u) = ‖μ_o(p,u) − t(p)‖ − ‖m(p,u) − t(p)‖`,

with `μ_o` the mean observed response conditional on calibrated colour, settings and output encoding. Signed residuals are estimated before their norms are taken, within the declared colour/settings cells, then aggregated with fixed weights. Unrelated colours are not averaged together, and the fitted contributor noise scale is not subtracted. Where the evidence cannot distinguish `μ_o` from observation scatter, significance is **unassessed**; the `>2σ` and five-effective-contributor policy is not permission to apply pixel SEM around a noise-biased raw gap.

**Off-grid support queries.** A probe support mask is not a continuous support field. The current artifacts hold per-probe matrices and flags, not an interpolation contract. Their recorded uncertainty applies at those canonical probe inputs and matching output context. Arbitrary captured colours do not inherit support through nearest-probe lookup, interpolation of support bits or interpolation of marginal covariance matrices; their model prediction and descriptive captured error may still be reported. Unsupported or unassessed does not mean "cannot render."

Assessed off-grid evidence uses the versioned queries `support(target, p)` and `support(realization, p, settings)`. The realization query accounts for its settings-adjusted Look input; target support cannot substitute for realization support. A query binds the model/evidence snapshot, calibration and gauge, evaluator, output encoding, clip and uncertainty rules, and evaluates every support predicate: density, independent clusters, prior bias, uncertainty and held-out discrepancy evidence. Outside the declared domain is unassessed, without clamping into an edge cell. A neutral input does not acquire hue coverage by being assigned hue zero. A producer may reuse retained covariance factors or already-produced bootstrap realizations at the actual query inputs; it does not start new fits per recipe image, and missing producer data stays missing.

**Joint moments.** Keep full matrices and shared dependencies. Prediction-minus-target uncertainty is

`Σ_(m−t) = Σ_m + Σ_t − Σ_mt − Σ_tm`,

and the cross terms matter when target and realization share calibration, family coefficients or fitting evidence. For systematic scalar-gap uncertainty, propagate the joint uncertainty of `(t, m, μ_o)`. Away from zero error norms, with unit error directions `u_m` and `u_o`,

`J = [u_m − u_o, −u_m, u_o]`, `σ_g² ≈ J Σ_(t,m,μ_o) Jᵀ`.

At zero norms that derivative is undefined; use an applicable joint replicate-based assessment or leave significance unassessed. Missing cross-covariance is not independence. Shared model uncertainty does not shrink with pixel count, re-renders of one RAW are not independent captures, and observation uncertainty is separate from published mean-response uncertainty. Systematic discrepancy moments and bias limitations are preserved, not relabelled as ordinary sampling covariance.

The current artifacts do not establish calibrated arbitrary-colour gap uncertainty. A nearby probe's support, pixel SEM, an assumed independence or a synthetic calibration/engine check cannot supply it. That limits assessed P6.7 claims; it does not block rendering, contribution intake, or existing valid probe-based matching and card paths.

1. **Fit (interactive path):** `Σ = H⁻¹ (c·M + Λ_prior) H⁻¹`, where `M = Σ_i g_i g_iᵀ` is the image-clustered meat (`g_i` is image *i*'s summed gradient at the optimum), `c` is the small-cluster factor applied to `M` alone, and `Λ_prior` is the curvature of every prior and penalty term in `H` (identity priors, smoothness penalties at the chosen λ, nuisance priors). Where data dominates this is the sandwich; thin regions, few clusters and flat clip derivatives tend to the prior variance.
2. **Bootstrap (published versions):** B = 32 warm-started refits on image clusters resampled with replacement; a cluster is one body's images, so within-body correlation is kept (a single-body pool is the image bootstrap).
3. **Cross-body disagreement:** covariance of leave-one-body-out predictions.

At a neutral internal chroma, `Φ` has no finite Jacobian, so covariance must not be pushed through it. `ChromaPaths::singular()` marks that location. Every path that computes probe uncertainty through `probe_uncertainty` (the interactive sandwich fit, the hierarchical publication pass and LUT import) keeps the finite terms, stores the unknown-variance convention on the masked chroma diagonal and leaves the probe unsupported; a bootstrap covariance for such a probe is not implemented. Elsewhere the published-output Jacobian is the exact `Φ`-chained derivative and the existing path is kept. A probe whose derivative or clip chain is not finite is degraded the same way instead of failing the fit.

Use the bootstrap fit covariance when available, otherwise the sandwich estimate. Combine fit and cross-body disagreement covariance in the same response coordinates; add model-discrepancy covariance in its measured post-clip coordinates. Scalar `σ_ΔE = sqrt(tr Σ_total)`.

Each probe carries both the **pre-clip** J′a′b′ the Look defines and the **post-clip** J′a′b′ in the reference output encoding (J′a′b′ → output RGB → soft clip with the realization's `k` → J′a′b′), each with its own 3×3 covariance. A camera realization's prediction carries its own post-clip covariance in the selected output encoding; the rendered metric keeps the target's reference context and the realization's output context distinct. Before model discrepancy is added, the post-clip fit covariance is `M_clip Σ_fit M_clipᵀ` plus the clip-`k` term, with `M_clip` the clip Jacobian. Matching and drift gating use the post-clip prediction and covariance; gating also checks pre-clip J′ and hue `h` where `pre_clip_reliable` is true (smallest per-channel clip derivative ≥ 0.25 and an invertible clip Jacobian).

**Prior-attributable bias:** `b_p = J_p H⁻¹ ∇_θ P_prior(θ̂)`, evaluated post-clip and reported as |b_p| in ΔE, with `P_prior` every prior and penalty in `H` (identity priors, smoothness at the chosen λ, nuisance priors, residual prior). The identity-prior gradient uses the centered tone prior, `W_T·(θ_T − m_T)`, so a tone coefficient at the colorimetric center contributes no bias, while the covariance keeps the unchanged curvature `Λ_prior`. A coefficient far from the identity Look, or one held by a smoothness penalty, carries a bias that its σ does not include.

**Model error:** cross-validation holds out whole bodies, or whole independent RAW/re-render groups for a single-body pool. Estimate the conditional residual vector in `(s, h_in, C_in)` cells, retaining each body's systematic residual energy before aggregation so opposite body errors cannot cancel. Separate uncertainty in that estimated mean from per-observation scatter; never subtract fitted `σ_v`. The discrepancy and its estimation uncertainty form a full 3×3 second moment in post-clip J′a′b′, separately for each output context. Add this moment directly to post-clip covariance, not through a second clip transform. Where the clip Jacobian is reliable and invertible, transport it back with `M_clip⁻¹ Σ_model,post M_clip⁻ᵀ` for pre-clip diagnostics; otherwise pre-clip identification stays false. The interactive path reuses the last published [`ModelErrorField`](../crates/looklab-fit/src/model_error.rs). A cell without independent held-out discrepancy evidence is unsupported even if its finite fallback covariance is small; that fallback is not evidence of zero model discrepancy.

**Support:** a probe enters support when post-clip `σ_ΔE ≤ 1.5`, `|b_p| ≤ 0.33·1.5` (tentative), the absolute density floor is met, at least `min_probe_clusters` images (tentative 3) carry non-negligible weight in its region, and independent held-out model-error evidence exists for its output context and color/exposure cell. It leaves support when `σ_ΔE > 1.65` or `|b_p| > 1.1·0.33·1.5`. The previous published version supplies the prior state for this hysteresis; the first version applies the enter rule only. Density, cluster and model-evidence requirements have no hysteresis and no shoulder exclusion. Unsupported probes are excluded from qualified matching objectives and hatched in plots; an experimental selection may use them under the rule below. Plain `fit` outside 4 to 64 images carries no held-out evidence, so the CLI writes an all-zero support mask there.

**Source-limited publication.** A native-family pool with fewer than two independent capture groups cannot hold out a fold and keep an image, so no cell can reach supported evidence even when the fit converges and passes every numerical and trust gate that does not need that evidence. A plain `fit` pool with no held-out fold returns an all-unsupported result instead of a fit error. Such a version is the zero-support subset of source-limited targets: it publishes as an experimental target with status `published`, no qualification report on the family head, scope `Experimental`, an active request for the missing independent captures, a firmware card marked experimental, and a version notice saying no region has an independently checked measurement yet. It stays listed, matchable and previewable. Convergence and every numerical gate are unchanged, and a qualified selection never uses this zero-support target; a source-limited target with supported probes keeps its existing same-camera qualified path ([§5.1](02-color-pipeline.md#51-qualification-claims-and-protocols), [05 §8.1](05-trust-and-integrity.md), [04 §1](04-matching-and-export.md#1-matching-levels)).

**Experimental probe rule.** Experimental matching uses the target's published supported probes. When the published support mask supports no probe, it uses the probes that meet the unchanged `σ_ΔE` predicate and the absolute density floor `FitOptions::density_floor` (0.01), and the result states how many images the estimate rests on. A target that stores no mask keeps the existing zero-floor rule. When that set is empty as well, the result is the existing no-supported-probes outcome. Thresholds do not change, and a qualified selection never uses this fallback.

**Absolute density floor:** the KDE counts are the tempered effective weight in each `(s, h_in, √C_in)` bin, `Σ n_eff/n_i · trust · density_w`, with the density weight normalized within each image. The floor is an absolute weight per bin (`FitOptions::density_floor`, tentative 0.01), so adding data elsewhere cannot lower a rare region's density below it.

**Small clusters:** for the sandwich path the meat is scaled by `G/(G−1)` and `σ_ΔE` by `t_{0.95,G−1}/z_{0.95}`, where `G` is the number of active image clusters in the fit (Cameron & Miller 2015, *J. Hum. Resour.* 50(2), eq. 10-11; MacKinnon & White 1985). Published versions use the bootstrap when available.

**Convergence:** final refinement requires both a relative objective plateau below `1e-3` over five accepted states and an undamped curvature-scaled decrement `E = ½ gᵀB⁻¹g ≤ 1e-3` nats on the joint metric. Here `g` is a freshly evaluated full-data penalized gradient over active, unpinned parameters, and `B` is the undamped joint IRLS/Gauss-Newton curvature over the Look coefficients, the active control-response coefficients, every image exposure and WB coordinate, the shared mode-offset and clip border and the free residual grid, including identity priors, nuisance priors, smoothness and control-response shrinkage at the frozen λ, and the residual prior. Each response coordinate contributes `2λ` to the diagonal floor `P₀`. The image blocks and the border are eliminated exactly with a two-level Schur complement before the solve, so the certificate resolves the same coupling the step does rather than a block-diagonal approximation. The outer conjugate-gradient preconditioner attempts the reduced principal block for each Look layer, then the unreduced block, the nominal diagonal correction retaining the group's off-diagonals, and finally the prior-only block. The zeroed-off-diagonal candidate is diagnostic-only until its unchanged-budget qualification passes. Its solve stops on an iteration work limit or on three consecutive chunks of iterations that do not lower the explicit residual. The residual grid contributes through its own matrix-free solve over the residual coordinates, which omits the residual/Look cross curvature. The reported value is the certified upper bound `½ zᵀBz + zᵀr + ½ rᵀP₀⁻¹r` for `r = g − Bz` and the diagonal prior floor `P₀`, so an inexact solve is accepted only when the bound is at most `1e-3`; a missing floor, a failed factorization or an unresolved solve reports nonconvergence. This is model-scaled stationarity, not an exact bound on remaining nonlinear improvement; it is invariant to invertible linear changes of coordinates over the joint space. Damping, step clamps and the joint direction safeguard do not enter the certificate. Final refinement runs two step models: the Student-t corrected analytic curvature `α JᵀJ − 2(ν+3)/q² (Jᵀr)(Jᵀr)ᵀ` with the existing Cauchy and box safeguards, and the positive Gauss-Newton curvature, including the neutral-anchored Bregman penalty's Gauss-Newton term of §3.9, as its own trust-region subproblem. The certificate above is unchanged and certifies stationarity of the objective regardless of which model produced the step; each model has its own radius and neither radius enters the certificate. Each radius adapts from its own candidate's realized ratio whether or not that candidate was applied. The Bregman penalty enters the objective and its gradient, and its Gauss-Newton term enters the positive model's curvature. The final-stage curvature operator uses all active rows when the pool fits the retained-row bound (8192 rows) and otherwise the deterministic `hessian_samples` subset scaled to the full data; `Diagnostics.operator_rows` reports which. Budget exhaustion and the sweep cap report nonconvergence. Warm-up stages use `early_samples_per_image` (default 64) on large pools and response fits; final refinement, the frozen response pass and uncertainty estimation use the full set. Plain `fit` applies the image/body K-fold model-error term to pools of 4 to 64 images. The worker records convergence in `ReleaseRecord.converged` and the version's gate summary. A fit that exhausts its budget returns a `FitState`, resumes on the next batch, and does not publish until final refinement converges.

### 5.1 Qualification claims and protocols

Qualification is a scoped empirical claim about a subject, not a body-wide badge. Every report names the subject (`SharedFamily` or `BodyRealization`), the exact family version or realization revision, the capability, the assessed settings and combinations, the output encoding, the colour and exposure cells, the firmware/epoch scope, the calibration and gauge dependencies, the evidence roles and the limitations ([03 §1](03-data-model.md)). Empty or unsupported assessment sets, missing control values, undefined checks and absent independent evidence cannot pass. Missing controls are unknown, not defaults; endpoint observations do not validate an interval; separate controls do not validate their Cartesian product; a candidate compared with itself, a fitted noise scale subtracted to manufacture evidence, and a synthetic engine check presented as camera evidence are invalid. Preparation availability, numerical validity, publication eligibility, empirical qualification and rooted trust are separate axes; a queued job, missing resource and failed assessment are different states. Promotion is not inferred from upload counts.

The three capabilities use the existing evidence and estimands:

1. **Native prediction.** The claim is the conditional mean rendering response for a named `BodyRealization(body, epoch, native_sim)` at declared settings, output encoding and colour/exposure domain, against a pinned family version and under the documented calibration and held-out nuisance treatment. It uses the model-error and support rules above and covers only the supported subset, not the whole probe set or camera. It is not an independently measured absolute scene-XYZ calibration. Mean-response uncertainty and future-JPEG observation noise remain distinct.
2. **Controls and matching.** P2.2 predicts held-out observed JPEG re-renders at assessed unseen settings within 1.5 ΔE median under the rendered metric and its recorded output context. P2.3 recovers settings within one step per control or predicts the true settings' error within 0.5 ΔE under the same rendered metric and output context ([09 P2.2, P2.3](09-roadmap.md)). These are observed-JPEG estimands, not latent mean-response uncertainty. Matching qualification composes the selected target's applicable evidence, the destination realization's CRM qualification and the actual requested domain; a cross-body claim additionally requires the applicable P1.9 evidence. A supported target does not make an experimental destination qualified.
3. **Cross-body admission.** P1.9 independently fits the common native sim on chart-anchored bodies under pinned calibration and gauge conventions and compares supported predictions; median disagreement above the tentative 2.0 ΔE bound fails ([09 P1.9](09-roadmap.md)). It qualifies agreement only on the recorded common support and coverage. Passing admits that evidence/domain to a new qualified-cohort family candidate, which must pass the common numerical, uncertainty, non-regression and publication gates; it does not copy an experimental realization into the target.

A `lut_defined` target is eligible for qualified matching through its existing import validity, frozen approximation, base-pin validity and inherited support/uncertainty plus current base applicability. Its stable `TargetEligibility` id names an immutable public proof snapshot for permalink pinning; current applicability is separate. It requires no native-prediction report or native-camera admission on the LUT. Destination and source camera realizations are assessed under their own `BodyRealization` scopes ([03 §1](03-data-model.md), [04 §1](04-matching-and-export.md)).

A source-limited native-family target's target-side eligibility is grounded in its published mean-response, probe, support and uncertainty artifacts plus applicable source/admission evidence. It does not require a native-prediction report on the family and is usable for the same-camera qualified loop before P1.9. Cross-body claims require their separate evidence.

A family's seed or reference cohort is the set of bodies whose eligible evidence established its canonical target. Seed membership is source-limited and is not cross-body qualification; the cohort may refit the same target as eligible data arrives under unchanged gates, so a source-limited target does not freeze until a second body supplies P1.9 evidence. P1.9 gates cross-body generalization, admission and transfer claims only. Same-camera native prediction and control qualification can pass while sensor independence remains unproven; publication status and rooted trust stay separate axes. A new body's cross-body membership remains experimental: it fits its realization against the pinned family version and cannot silently join the seed cohort. Admitting it to the qualified cohort requires the scoped P1.9 agreement and the applicable engine prerequisite; an update from an existing seed body does not require a new cross-body admission. One family identity, its source limitations and its lineage are preserved across both.

P4.3 known-truth coverage is ENGINE qualification, evaluated once per fitter/evaluator/model version on synthetic or otherwise known-reference evidence: true error inside the 90% band at least 85% of the time on held-out bodies, at the existing thresholds ([09 P4.3](09-roadmap.md)). It is not a latent-truth requirement for each incoming camera. Missing engine qualification blocks qualified multi-body activation; missing camera evidence leaves that camera/domain experimental. Calibration source and confidence remain dependencies and limitations, not an absolute-accuracy badge.

Qualification, promotion, invalidation and evidence requests run through the existing worker jobs and publication request. These are small versioned protocols implemented once, not a per-camera policy language. Broad intake and experimental publication do not require a camera allowlist; an experimental realization is published only when numerically valid and gate-passing, and a provisional or experimental label is not publication permission. A failed or nonconverged candidate may expose permitted diagnostics, not accepted render or match artifacts. Missing evidence produces a specific wanted shot or resource request, not a moderation promotion. Root admission, human trust, physical captures, schema proposals and exceptional re-anchors remain human. Scoped qualification and publication eligibility are in [05 §8.1](05-trust-and-integrity.md#81-scoped-qualification); isolation of experimental influence, scoped automatic promotion, failed-replacement retention and dependency fencing are specified in [05 §8.2](05-trust-and-integrity.md#82-experimental-isolation) and [05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation); qualified versus experimental consumption is in [04 §2](04-matching-and-export.md). Implementation rationale is in [model qualification plan §5](model-qualification-plan.md#5-three-qualification-capabilities).

## 6. Probe set (v1)

Versioned in `data/probes/vN.yaml`, generated deterministically, in adapted scene XYZ (D65). The v1 reference inputs (`data/charts/colorchecker24_2014.json`, `data/probes/pointer_gamut.json`) are parsed with correctly rounded `f64` (`serde_json` `float_roundtrip`), so regeneration is independent of the cargo package selection.

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

**Fitter trace:** setting `LOOKLAB_FIT_DIAG` writes the staged solve's per-sweep cost, block gradients, joint-solve outcome and stationarity bound to stderr. It changes no numerics and is for local investigation only; the `Diagnostics` record carries the same quantities for released fits.

**Frozen-step audit:** `bounded_fit_eval --diagnose-fixed-linearization <state>` under the `synth` feature binds the input hash and evaluates one fixed coefficient/σ/λ state at its saved radius. It uses at most one linearization, two objective-only trials, 2044 inner Hessian products including refreshes, and four post-solve products. The production per-attempt allowance is 4096: 2044 for the analytic solve plus up to four post-solve products, and 2047 for the positive solve plus one realized-step product. The report distinguishes preconditioned, recursive and fresh Euclidean residuals, counts refreshes/restarts, and separates the ordinary TR/Cauchy winner from the final predicted-decrease ranking with the guarded box alternative. Inactive box alternatives have no separate product or objective trial.

Production `WorkLimit` returns an error and discards its proposal. The diagnostic can compare a finite retained iterate without changing that disposition or claiming stationarity. Realized candidates use fresh products and unchanged feasibility/acceptance rules; recursive cached progress is not a fresh score. With no explicit forcing-gradient reference, the current resume rule resets it to the fresh gradient norm. The report labels that case a first-resume attempt, not an exact continuation of the uninterrupted run.

**Fallback A/B:** `bounded_fit_eval --diagnose-fallback-ab <state>` compares the baseline diagonal fallback with the zeroed-off-diagonal candidate on one shared linearization. The input must be the bound checkpoint and carry its saved trust radius; the runner verifies the checkpoint SHA-256 and requires a source generation. Baseline runs first; the candidate receives the remaining shared inner allowance capped at the production 2044, within at most 4088 inner products across both arms plus eight post-solve products. Each arm starts from the saved θ/σ/λ/mask/radius with a first-resume reference and uses the normal realized TR/Cauchy/guarded-box selection, which evaluates the ordinary winner and, when the box alternative out-predicts it, the box alternative as well. The sample ceiling covers one trial per arm, so an arm that evaluates both candidates is reported `inconclusive`. An arm that terminates successfully on its last permitted inner product is not ranked or scored; either arm exhausting its allowance makes the comparison inconclusive. Setup and prediction failures retain the completed linearization pass, sample and product ledger, and the actual trust-region trace. The report records every group's fallback branch, the selected winner, fresh prediction, actual decrease, gain, metric norm, largest coordinate, clipping, termination, the complete ledger and the comparison verdict. Baseline reproduction is checked against the frozen audit's full recorded precision: cost, prediction, gain, termination, inner and post products, radius, largest coordinate, selection, clipping and the trust-region trace. The verdict is `candidate-accepted` only when that record matches and `B > A + τ` with an accepted, feasible candidate; otherwise it is `candidate-rejected`, `tied`, `inconclusive` or `baseline-mismatch`.

## 8. Implementation notes

- f64 for all fitting and color math; f32 only in sample storage.
- **Determinism (native = WASM bit-identical):**
  - All transcendental math in core crates goes through the `libm` crate (`pow`, `exp`, `ln`, `sin`, `cos`, `atan2`, `tanh`, …). Ban the `f64` methods with clippy `disallowed_methods`. `sqrt` (correctly rounded) and basic arithmetic are fine.
  - The Look evaluation path uses scalar code or fixed-order reductions only: no `faer`/`pulp` or other runtime-dispatched SIMD, no fast-math, no FMA contraction differences. A test compares native and WASM outputs bit for bit on fixtures.
- Golden tests vs colour-science for every color function (1e-6 relative; CAM16 inverse 1e-5). Note: colour-science's CAM16 defaults compute D, so golden vectors must pass `discount_illuminant=True` where the reference conditions apply.
- Look evaluation target: < 3 µs/color native for layers + residual, including the closed-form `Y_ref` solve (§3.3).
