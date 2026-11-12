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
| C | body only (uses the crowd model); optional verification loop | Recommended settings + predicted error map + reachability status |
| C+ | hacked firmware that implements §8 | Card file; verification through in-camera renders (§5, §8.4) |
| C+ | software interface profile | Baked artifact + validation report |

Every result states the scope it was produced under, and each output path names only the dependencies it uses. **Qualified matching** uses only dependencies whose qualification covers the requested body, epoch, native sim, settings and combinations, output encoding and measured colour/exposure domain. **Experimental selection** is the explicit path offered when no qualified result covers the request, and any result, card, export or download it produces carries that status. A source-limited target with no supported probe is consumed only through that path: its settings result names the number of images the estimate rests on, it offers a firmware card marked experimental, and a software export refuses it because export validation needs a supported probe. Qualification is composite within a path: stock settings need the target's applicable evidence (native mean-response or `lut_composition` eligibility), the destination realization's control-response qualification and the requested domain, and a cross-camera settings claim also needs cross-body admission ([02 §5.1](02-color-pipeline.md#51-qualification-claims-and-protocols), [05 §8](05-trust-and-integrity.md#8-release-gates-and-rollback)). A card file needs a publishable valid target and a manifested `card.llk`, not a camera realization, control-response model or their convergence; a qualified card also needs a supported probe when the target publishes a support mask, while a source-limited target with no supported probe gets a card marked experimental and keeps its settings, preview and experimental match ([02 §5](02-color-pipeline.md#5-uncertainty)). A software export needs the dependencies its profile and insertion point declare, such as the camera transform for camera-RGB input, and no control-response model the pipeline does not use. Native-family target eligibility is grounded in the published family mean-response, probe, support and uncertainty artifacts plus its source and admission evidence, not a circular family projection or a family-level realization report. A `lut_defined` target is not a native-family fitting subject and is never given a native-prediction report or camera admission of its own; its target eligibility is an immutable `lut_composition` eligibility snapshot, `eligibility:<id>`, with `CurrentTargetEligibility` as the separate derived current status, and its camera source bases and destination qualification apply normally where a settings or export path uses them ([02 §3.8](02-color-pipeline.md#38-lut-defined-looks), [03 §1 qualification records](03-data-model.md#modelsubject-realizationrevision-and-qualification-records)). A supported target does not make an experimental destination camera qualified.

Preparation readiness, mapping confirmation, root admission and upload counts are not empirical qualification. A body can be preparable, discoverable and contributable while its realization stays experimental, and a numerically valid, gate-passing experimental realization remains usable through the experimental path ([03 §1](03-data-model.md#1-entities)). Default-only evidence qualifies no nondefault control, and observations at numeric endpoints qualify no untested interval or control interaction. Experimental status never bypasses convergence or a release gate: a nonconverged or gate-failing target or realization candidate exposes diagnostics rather than an accepted result for the path that depends on it, and a camera realization failure does not withdraw an otherwise publishable target card.

Source-only LUT versions ([03 §5](03-data-model.md#5-published-static-data)) are discoverable through the gallery and visual comparison with their declared-base limitations. Their preview LUT composes the original LUT on the recorded identified approximation or neutral fallback, and that composition is shown even when no settings fit exists. They provide no quantitative target comparison, stock-settings match, firmware card or software export. Experimental settings or a download appear only after a first fitted representation passes convergence, the existing 1.0 ΔE supported-probe p95 accuracy requirement and supported-probe checks, and the output keeps the remaining base and destination-camera limitations. Neither an explicit version pin nor a catalog selection turns their preview into a layered target.

## 2. Realization model on a body (stock firmware)

The realization of base native sim *k* on body *B* at settings *u* has two parts:

```
θ(k, u)       = θ_L(k) + δ_fw + Σ_j φ_j(u_j) · D_{B,k,j} + Σ_{(j,l)∈I} φ_j(u_j) φ_l(u_l) · D_{B,k,jl}
input effects = WB gain ω_B · W(u_wb)  and  exposure 2^(ξ_{B,m(u)} + EC(u))
```

- `θ_L(k)`: the Look family that sim *k* realizes ([02 §4.2](02-color-pipeline.md)). The family/seed stage fits θ_L jointly with D from eligible native data and defines the canonical family estimate ([02 §4.3](02-color-pipeline.md)). A conditional realization stage holds the pinned family version fixed and fits only that `(body, epoch, sim)` realization, so another body's experimental fit cannot update the target.
- `φ_j`: a per-control basis. Integer/decimal controls get one coefficient vector per step offset from default (piecewise-linear, so nonlinear responses can be learned). Enums get one vector per non-default value.
- `D_{B,k,j}`: restricted to the layers listed in the control's `affects` ([03 §1](03-data-model.md)). This is a strong prior that keeps the model identifiable with little data.
- `I`: sparse interactions, declared in the schema or added when residuals demand. Default: none.
- **`wb` controls** act as a 2-DOF gain on camera RGB before the Look. **`exposure` controls** (mode and EC) act through ξ and EC. Neither changes θ.
- Learned from re-render series (most informative), all native library data, and recipe uploads (recipe stage only, [02 §4.3](02-color-pipeline.md)). Uncertainty comes from the image-clustered sandwich estimator. Settings far from observed data get inflated uncertainty.

**v2 storage.** `T` and `N` deltas are coefficient offsets. `G`, `R` and `Λ` deltas are a ten-term unit-norm expansion over the layer's plane and `C > 0` volume (`C = 0` plane, uniform `C > 0` volume, hue cosine and sine, `s`-linear, `C`-linear, and four Gaussian hue bumps), which keeps a control with many steps to a few coefficients per step while allowing hue-localized responses. The layer vector also carries the near-neutral core ([02 §3.3](02-color-pipeline.md)); control deltas do not move it. The fit regularizes each control's expansion with shrinkage toward zero plus second differences across adjacent numeric steps; generalized cross-validation chooses the weight. Deltas at the default are exactly zero, and numeric interpolation is piecewise-linear between stored steps. `support` records the observed values; a probe or candidate beyond it keeps its prediction and has the response covariance multiplied by `1 + d`, where `d` is the step distance outside the observed range (unobserved enum values use 2). [03 §1](03-data-model.md) gives the `crm.json` layout.

**Per-sim inputs.** A body epoch declares its native sims. Each sim with published data supplies one base realization Look (its Look family evaluated on that body) and one control-response model, both scoped to `(body, epoch, sim)`; the model's predicted covariance is optional, since the trace term is only a penalty. Each sim's base realization pins that sim's own native family version, which need not be the target Look's family. A match may target any other native family or a `lut_defined` Look; the bundle entries stay realizations of their own sims' families, and only the selected target supplies the goal.

**Match bundle revisions.** A `MatchBundleRevision` is keyed by `(body, epoch)` and has its own immutable revision and content identity, manifest and ordered per-sim entries ([03 §1](03-data-model.md#1-entities), [03 §5](03-data-model.md)). Its id is the lowercase hex SHA-256 content digest. It publishes under `data/bundles/<body>/<epoch>/<bundle-id>/manifest.json` plus its artifacts. It pins the schema snapshot, input transform and body realization parameters, and for each ranked sim the `RealizationRevision`, the sim's own native family version, the control-response model and the optional covariance artifact. The target Look version is selected separately by the request or permalink; a bundle revision is not owned by a target Look version. A camera-only update, a new body or a new sim publishes a new bundle revision through the existing jobs and index and does not create a target Look release. A new revision may change the available sims; older bundle revisions and their manifests stay historical, immutable and resolvable. Later qualification over unchanged bytes adds a report association to the same revision and rewrites neither bytes nor manifest. Bundle entries are source refs only, with no status or qualification ref. A qualified selector composes the current reports and `CurrentTargetEligibility`, ranks only covered settings and sims, and checks current applicability separately.

**Selection scope.** Every listed sim whose current scope covers the request is a candidate for the settings optimizer; a sim without a published realization or response never enters the ranking. A selection uses one bundle revision whose schema, transform, body parameters and realization revisions share the required calibration and gauge identity; incompatible sibling revisions are excluded rather than mixed. A qualified selection ranks only entries qualified for the requested domain and never silently acquires an experimental entry; an experimental entry appears in a result or download only through the explicit experimental path. Bundles published before bundle revisions existed remain resolvable as legacy target-owned bundles under their recorded provenance and gain no new entries.

The CLI uses the same per-sim assessed optimizer as the browser. Selected matching and replay authenticate the bundle inventory and pinned report/eligibility artifacts against `--publication`; a new match also authenticates its target artifact and current output eligibility. Qualified searches use each sim's own complete realization identity, firmware, WB mode, assessed settings and common supported probe cells. `--controls-report` names a published report artifact and adds its immutable pin; other pinned controls reports load from the publication. Independent camera bundles supply their per-sim realization revisions without a global `--realization-version`; unbundled and legacy target-owned diagnostic inputs require that explicit revision and claim no current qualification. Verification applies the same scoped searches before and after local calibration, preserves historical status notices, and pins the resulting winner's own realization-parameter revision.

**Targets and realizations.** A camera realization for a native-family target is fitted against one pinned canonical family version ([03 §1](03-data-model.md#1-entities)); it is not a second target Look. A `lut_defined` target keeps its frozen composition and base pin ([02 §3.8](02-color-pipeline.md#38-lut-defined-looks)); its target eligibility is an immutable `lut_composition` eligibility snapshot, `eligibility:<id>`, with `CurrentTargetEligibility` as the separate derived current status ([03 §1 qualification records](03-data-model.md#modelsubject-realizationrevision-and-qualification-records)), and matching against it uses the destination realization normally and never requires a native-prediction report or camera admission on the LUT itself. A higher-version camera-local diagnostic fit, including an independent fit made for the P1.9 comparison, never becomes the family target, the previous family release or the anchor. A realization revision can change without redefining the target, and experimental publication cannot move target selection.

A source-limited family refits its existing seed cohort as eligible evidence arrives under the unchanged gates; it does not wait for a second body's P1.9 evidence. When a family has no target, eligible native evidence from any supported body may seed one provisional canonical estimate under the existing trust, resource and gauge policy; the reference body is not an allowlist, and seeding grants neither root authority nor evidence of sensor independence. Another body fits against the pinned family version and joins that cohort only through scoped admission. Each revision records its source limitations and lineage, so a camera added later does not silently become part of the seed cohort. A `lut_defined` Look keeps its frozen composition: native-family cohort admission never retrains its coefficients ([02 §3.8](02-color-pipeline.md#38-lut-defined-looks)).

## 3. Settings optimizer

Objective for a candidate (k, u):

```
E(k,u) = Σ_p w_p c_p · ΔE_rendered(p; L, Real_{k,u})²  +  β Σ_p w_p · tr Cov_rendered(k,u,p)
```

- `p` is one fixed calibrated scene input per probe. Exposure compensation and WB shift adjust the realization, never the target or `p`.
- `ΔE_rendered` compares the target rendered through its pinned reference encoding and clip with the realization rendered through the selected output encoding and its realization clip, both decoded once into the common CAM16-UCS frame ([02 §5](02-color-pipeline.md#5-uncertainty)). There is no inverse clipping of the observation and no second soft clip of a decoded sample.
- `w_p`: probe group weight times the user's emphasis (balanced / skin / landscape / product).
- `c_p = 1/(1 + σ²_L(p)/τ²)`, with τ = 1 ΔE and `σ_L` the target's post-clip rendered uncertainty. Unsupported probes are excluded.
- β = 0.25 penalizes extrapolation.
- `Cov_rendered` is the realization's predicted covariance in the same rendered metric and output context.
- Emphasis multiplies the [02 §6](02-color-pipeline.md) group weights:

| Group | balanced | skin | landscape | product |
|---|---|---|---|---|
| Gray ramp | 1.0 | 1.0 | 0.8 | 1.2 |
| Chart patches | 1.0 | 1.0 | 1.0 | 2.0 |
| Hue grid | 1.0 | 0.8 | 1.25 | 1.0 |
| Skin | 1.0 | 3.0 | 0.5 | 0.8 |
| Memory colors | 1.0 | 1.2 | 3.0 | 1.0 |

**Budget: < 2 s for a full search in one WASM worker** (p95 across fixture bodies). The same budget applies everywhere ([09 P2.3](09-roadmap.md)). To meet it:

1. **Prune base sims.** Score every native sim exactly at defaults on the probe set, with the best EC and WB found by 1-D sweeps on a 64-probe coarse subset. Keep the top 3; only they build the per-sim linearization and run the search.
2. **Search on a probe subset.** Use 500 probes, stratified by group and hue, with weights rescaled. Rescore the final top 5 on the full probe set.
3. **Per-probe linearization.** For each kept base sim, precompute at its default the per-probe rendered output `o_p` and the Jacobian `J_p` with respect to the coefficients D can move, plus the WB and exposure input effects. Then precompute every control value's contribution `c_{p,j,v} = J_p D_{B,k,j} φ_j(v)` (a 3-vector). Evaluating a candidate is then a handful of 3-vector adds per probe. Exactly re-evaluate the top candidates. If linear and exact rendered predictions differ by > 0.3 ΔE median, re-linearize at that candidate and continue (a trust-region style step).
4. **Low-rank predicted covariance.** Rank-20 eigen-approximation of Cov(D); per-control-value trace contributions precomputed in the same additive form.

Cov(D) arrives as `looklab-crm-cov/1`: the control-response model's `parameter_ids`, row-major covariance over them (D and WB). The optimizer splits it by `(control, value)`, eigendecomposes each block, keeps the 20 largest positive eigenvalues, and applies `Support::inflation(settings)` to the resulting trace.

The optimizer scores only the rendered metric of §3. Pre-clip J′a′b′ remains a fitting and diagnostic quantity; it is not the objective, the reported match error or a fallback when the post-clip frame is inconvenient. A new match takes the target as `TargetAppearance::Rendered(OutputPin)`; `TargetAppearance::PreClip` is diagnostic only. The target is rendered once per pinned version and output context, and a candidate cannot refit it, change its encoding, or move its scene input `p`. Every search records its `ScoringMetric` and `OutputContext` ([03 §1](03-data-model.md#rendered-metric-output-context-and-assessment-status)); a candidate scored under another metric or context is not comparable.

A point whose composed render derivative is unavailable is omitted only from the additive linear guidance. The exact objective, pruning, full rescore and declared coverage still include every point, and an empty guide still seeds exact evaluation. If an active covariance is unavailable, the candidate is unrankable under the full objective rather than scored with a zero penalty; an inactive covariance contributes exactly zero. A missing band is unassessed. `NoRankableCandidate` reports that no candidate could be ranked under the full objective; it is not an unsupported camera and not a proven unreachable target.

**Search:** for each kept base sim, run staged integer coordinate descent following the schema's `solve_plan` (tone stage, then color stage; each control swept over its range; iterate to convergence). Then 3 perturbation rounds (random joint ±1–2 step moves on 2–3 controls, accepted if better). Keep a beam of 5 per sim and return the top 3 overall. Defaults: 500-probe subset, 5-probe full rescore, 4 coordinate passes, 1 re-linearization, 2 exact warm restarts for the leading sim, seed supplied by the caller.

Large nonlinear moves, such as a two-stop tone change together with exposure compensation, can be pruned by a linearization at default. The leading sim therefore gets a coarse exact coordinate descent on 64 probes, plus two random-restart descents on 48 probes, and the beam seeds are exact-scored on the full set. On 3 synthetic sims and the 1057-probe set a clean native release run measures median 0.72 s, p95 0.77 s; the same example under wasm32-wasip1 on wasmtime measures median 1.10 s, p95 1.26 s. Runs under concurrent build load reach a native p95 near 2.0 s.

## 4. Settings card and permalinks

The **settings card** shows:
- The base sim and its realization version, and every control value, in the camera's menu order and wording, **including exposure compensation and WB shift** when they help (e.g. "this body renders 0.5 EV darker in DR200: set EC +0.3 and WB shift R+1").
- Predicted ΔE median / p95 with the uncertainty band, in the rendered metric and named `OutputContext` ([03 §1](03-data-model.md#rendered-metric-output-context-and-assessment-status)).
- The band is `sigma_band`, the mean-model marginal only; `joint_uncertainty` is `AssessmentStatus::Unassessed` in the current core because no cross-covariance is available.
- The requested recipe output encoding (`srgb` by default, or `adobe-rgb`), pinned as `output.camera`; the CLI, WASM, API and UI expose the same requested setting. A qualified match requires the pinned controls report's `scope.output_encoding` to match.
- A per-layer residual summary in natural language.
- Regions with `ReachabilityStatus::NoRecipeFound` within the declared search, e.g. "Cyan chroma can't be reduced enough; the best remaining error is 3.8 ΔE." A search miss is `NoRecipeFound`; the implementation emits no `Unreachable` status, and proving unreachability requires a valid bound or exhaustive coverage of the declared legal candidate space.
- The covered part of the declared assessment set as `ReachabilityStatus::Partial` or `Unassessed`. Partial coverage is reported as partial; unsupported points stay in `declared` and do not disappear from the denominator.
- A `ReachabilityStatus::Reachable` claim names one legal settings vector in `Reachability.witness` whose exact post-clip error is at most `REACHABILITY_TOLERANCE_DE = 2.0` at every assessed point in the declared assessment set, with applicable model support and settings/domain evidence ([05 §12](05-trust-and-integrity.md#12-recipe-channel-and-bounties)). `evaluated` and `within_tolerance` are descriptive rendered results; `assessed` and `within_assessed` count only points with an applicable target and realization/settings-domain witness, and `within_assessed` drives `status`. Without that domain, retain `declared` and all raw `evaluated`/`within_tolerance` counts, set `assessed = 0` and `within_assessed = 0`; a raw descriptive miss may still report `NoRecipeFound` (not unreachable), while all raw points within tolerance without domain report `Unassessed`. Only `within_assessed` can justify `Partial` or `Reachable` under applicable support. Missing target uncertainty or support leaves confidence `Unassessed`, not fully supported. Different settings reaching different probes do not establish one usable recipe.
- Confidence, which is low when the control-response model is thin.
- The selection scope: qualified, or experimental with the blocker that prevented a qualified result and the evidence that would resolve it. Scope and confidence are separate; a scoped pass does not claim absolute calibration, and a thin model does not make a scoped pass invalid. A `NoRankableCandidate` result reports no candidate rankable under the full objective; it is not an unsupported camera and not a proven unreachable target.

The card's error values are post-clip rendered errors, not pre-clip model distances. It carries `metric`, `output` and `reachability`; a card or result without them is historical and keeps its pre-clip meaning. Card `.llk` bytes and the pre-clip firmware semantics of §8.1 do not change.

**Permalinks** pin everything needed to reproduce the result: the target Look version, the `MatchBundleRevision`, epoch map version, probe set version, optimizer version and the user's emphasis. A rendered link (format 4) also pins its `ScoringMetric` and `OutputContext`. The bundle revision pins the schema snapshot, input transform, body realization parameters and every ranked sim's `RealizationRevision`, own family version, control-response model and covariance artifact. All identities are immutable, so a permalink always reproduces.

Formats `looklab-match/1`, `/2` and `/3` keep their canonical bytes and historical pre-clip meaning. They parse as `ScoringMetric::LegacyPreClipV1` with no output context, require optimizer `looklab-match/1`, reject `metric` and `output` fields, and are never silently converted or requalified. The legacy identity records the old pre-clip target/prediction comparison against decoded post-clip JPEG; it does not assert inverse clipping of old observations. A rendered link uses `looklab-match/4`, requires optimizer `looklab-match/2`, and records both the metric identity and the output context; replay routes by those recorded identities rather than by the current optimizer or encoding.

The pinned bundle revision and immutable report identities fix the assessment context. A result records its selection mode and the full applicable report set for the target and for every ranked sim in the ordered bundle, each report naming the capability and domain it assessed. Eligibility can change pruning and rank across the whole ordered bundle, so the winner's reports alone do not reproduce the selection. Replay reproduces the recorded mode and reports, and queries current applicability separately: a pinned report that was withdrawn or superseded produces an invalidation notice, and a current report never substitutes for the historical one. An experimental selection with no applicable report records no reports and replays as experimental and unassessed.

A permalink does not store the selected settings. Replay loads the full ordered sim list from the pinned bundle revision's verified manifest, applies the permalink's calibration overlay to the original per-sim models, and reruns the same optimizer with the same options and seed, so the winner keeps its original array position, pruning rank and search context. A link whose bundle revision is unknown, or whose body, epoch or manifest does not match the link, is rejected rather than replayed against a single sim, and a link whose pinned sim, realization revision, model or covariance differs from the bundle is rejected rather than recomputed. The target Look version is resolved separately from the bundle and never changes it.

The canonical encoding is a single line of `key=value` fields joined by `|`, starting with `looklab-match/1`, in this order: `look`, `base`, `crm`, `input`, `realization`, `epoch_map`, `probes`, `optimizer`, `emphasis`, `seed`, then optional `target_file`, `base_file`, `body_file`, `crm_file`, `transform_file`, `covariance_file`, `calibration`. Look and base are `slug@version`; CRM is `body@epoch@sim@version`. Values are percent-encoded. An empty calibration uses `looklab-match/1`; a non-empty one uses `looklab-match/2` and appends `calibration`, which holds `sim@control=factor` entries joined by `;` with the names percent-encoded inside the field. Factors are finite and lie in 1e-4 to 1e4; a link carries at most 32 sims with 64 controls each and a 128 KiB calibration field. Parsers reject calibration in a v1 link, unknown fields, duplicate sims or controls, and factors outside that range; replay rejects a calibration naming a sim or control the bundle does not supply. A format 1–3 link carries optimizer `looklab-match/1`; a format 4 link carries `looklab-match/2`. `looklab match` writes this string; `looklab verify --permalink` resolves the optional file fields relative to the permalink's directory and rejects an unsupported optimizer or a mismatched epoch map, transform version, or model version.

A link with recorded selection context uses `looklab-match/3`, carries the same fields in the same order, and appends `bundle`, the bare opaque `MatchBundleRevision` id, a lowercase hex SHA-256 content digest that pins the exact manifest, also written `bundle:<id>` where a typed reference is needed; `selection`, either `qualified` or `experimental`; and `reports`, a semicolon-separated set of `subject=<ref>` entries. The subject is the target `look@version` or `realization:<RealizationRevision.id>` for a ranked sim, so identity never comes from a mutable version ordinal. The ref is `report:<id>` or `eligibility:<id>`, a snapshot id of an immutable public proof summary; mutable current applicability is never encoded in the link, `CurrentTargetEligibility` is resolved separately, and `bundle:<id>` may appear only to pin a bundle association. Capability and scope belong to the referenced snapshot, so multiple entries per subject express multiple capabilities and scopes. Every subject and ref component is percent-encoded before joining, and the joined value uses the outer field escaping. A v3 link always carries `bundle` and `selection`; `calibration` and `reports` may be empty. Parsers reject a v3 link without a bundle or mode, a qualified link without its applicable target entry or required ranked-sim entries, or an entry whose ref does not match the pinned artifacts. An experimental link with an empty `reports` field is the explicit absent-evidence record and replays as experimental and unassessed. A `looklab-match/1`, `/2` or `/3` link keeps its historical target-owned replay semantics, parses with `ScoringMetric::LegacyPreClipV1` and no output context, and is labeled unassessed; no link is silently converted or requalified. Later reports over unchanged bytes add a current association and leave the recorded context intact. Card and Look bytes and the upload envelope do not change.

A link produced under the rendered metric uses `looklab-match/4`, carries the earlier fields in the same order, and appends `metric=looklab-scoring-metric/rendered-post-clip/1` and `output=target=<encoding>@<clip_k>;camera=<encoding>@<clip_k>`, the canonical `OutputContext` string. The target pin is the target version's pinned reference encoding and clip; the camera pin is the selected output encoding and the realization clip. The v3 `bundle`, `selection` and `reports` fields are required only when a selection is present; `calibration` remains optional. A format 4 link requires `metric`, `output` and optimizer `looklab-match/2`; parsers reject a missing or legacy metric, a missing or malformed output context, and a format 1–3 link carrying either field. Replay routes by the recorded metric and output context, so a historical link never acquires the rendered meaning.

A non-identity local calibration overlay, with any factor different from 1, makes the search experimental. Parsers reject `selection=qualified` with such an overlay. Retained reports describe the pinned baseline only and do not qualify modified coefficients. A qualified modified realization must instead be independently assessed and pinned as a new published realization, with an identity local overlay.

`looklab verify` loads a bundle revision from `--bundle`, otherwise from the pinned bundle revision's manifest beside the permalink, the pinned model, the base realization or the covariance. The descriptor, schema, transform and every sim's artifacts must verify against the bundle revision's manifest. Every pinned body, transform, base, CRM and covariance source must hash to the selected bundle revision's manifest-verified content; relocation is allowed only when the bytes match. A permalink that names bundle artifacts but has no bundle revision beside it is rejected rather than replayed against one sim. Each round composes the newly learned per-control scales into the cumulative overlay for the fitted sim, keeps every other calibrated sim, and writes the next permalink for the second suggestion's sim with that sim's original CRM version and sources from the immutable bundle revision. The materialized CRM file is a convenience copy and is not referenced by the permalink; verify refuses an output path that has the same filesystem identity as any loaded original CRM.

`looklab match --look <target.looklab> --body <schema.yaml> --crm <crm.json> --base-look <sim.looklab> --transform <transform.yaml> --realization-version <n> [--output-encoding srgb|adobe-rgb] [--emphasis skin]` prints the settings card and the permalink for one sim. `looklab match --look <target.looklab> --bundle <bundle-dir> [--output-encoding srgb|adobe-rgb] [--emphasis skin]` reads a published bundle revision and ranks every sim it lists; the bundle supplies its own manifest-verified schema, transform and per-sim models, so `--body`, `--transform`, `--crm`, `--base-look` and `--covariance` are rejected, and `--epoch` must match the bundle's epoch. A camera bundle revision supplies each ranked sim's own realization revision, so no `--realization-version` is passed; a legacy target-owned descriptor still requires one. Without `--selection`, `--firmware` and `--wb-mode` are rejected rather than recorded. `--synthetic` substitutes the target Look as the base realization and the identity transform; it is required when those inputs are absent and cannot be combined with `--bundle`. `looklab verify --permalink <file> --observed <dir> [--bundle <version-dir>]` runs one loop iteration, prints the second suggestion's settings card, and writes an updated local CRM, a new permalink and a replay JSON carrying the second settings. Without a bundle, verify takes `--look`, `--body`, `--crm`, `--base-look`, `--transform` and `--covariance` for a legacy single-sim recipe. Re-render at the second settings and run `verify` with the new permalink to continue.

`--output-encoding` defaults to `srgb`; the CLI prints `recipe output encoding: <token> (requested setting)` and pins it in the rendered permalink's `output.camera`. The requested output encoding is a recipe output choice, not a schema control. The permalink and card use schema tokens `srgb` and `adobe_rgb`. A qualified match requires the pinned controls report's `scope.output_encoding` to match. Replay keeps the permalink's own pin, and a legacy link remains pre-clip with no output context.

## 5. Verification loop (local, anonymous OK)

1. The app picks 2–4 of the user's RAWs (or asks for specific shots) covering the regions with the most predicted uncertainty.
2. The user re-renders them in camera at the recommended settings and drops in the JPEGs.
3. The client fits the observed realization, compares it with the prediction, updates a **local** copy of D (a few LM iterations) and re-optimizes. Repeat 1–3 times.
4. At the end, offer to contribute (this creates a credential if there isn't one). Verification renders are re-render series plus recipe data.

The first implementation updates one scale factor per non-default control with two Gauss-Newton iterations (ridge 1e-3, factor clamped to 0.05–20), then re-runs the full search. Selection ranks candidate RAW sample sets by mean `tr Cov_rendered` at the recommended settings and keeps 2–4. Stored `in_rgb` is camera RGB before the body realization effects; `A_c · in_rgb` is the unmodified scene, and the realization applies WB and exposure once for prediction and target measurement. Verification compares the observed JPEG, decoded once through its declared encoding into the common CAM16-UCS frame, with the rendered prediction under the same `OutputContext`; the local fit uses the signed residual `o − m`, and neither side is inverse-clipped or soft-clipped twice. A rendered verification rejects observed sample headers whose declared output space differs from the pinned camera encoding through the central output validator. Before fitting, verification replays the full ordered sim search from the bundle to recover the settings the match showed; the fit runs at those settings, and the second suggestion is ranked by the same full search over the updated models. The fit returns the cumulative factor per control, which the next permalink stores per sim, so the next round replays the learned models even when another sim becomes the winner. `looklab verify` reports the first suggestion's error against the observed re-renders in the rendered metric; measuring the second suggestion needs a re-render at its settings. Verification is local evidence about one camera's realization; it does not promote a qualification claim, and a contributed verification render enters the normal evidence path.

Verification writes the resulting selection mode and the rendered `ScoringMetric`/`OutputContext` into the next permalink. A non-identity cumulative overlay changes a previously qualified search to experimental; local measurements do not extend the baseline's community assessment to the modified response.

Verification observability boundary: a controlled recovery claim requires an identifiable response curve for every varied control; a sparse zero-response case cannot learn unseen values and does not establish recovery. This does not weaken P2.4's acceptance criteria.

## 6. Interface profiles (software pipelines)

`data/interfaces/<id>.yaml`. Interface profiles describe software pipelines that apply a LUT at a known point: raw developers and editors. Their authors submit profiles in the app (as YAML or through a form), together with a round-trip report generated at submission ([08 §4](08-ui.md)). Users pick the pipeline from a list. Hacked camera firmware does not use profiles; it implements the card format (§8).

```yaml
id: example-app/lut3d-v1
name: "Example editor 3D LUT module"
version: 1
bodies: [vendor/model-a, synthetic/looklab]  # empty = body-agnostic
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

**`bodies`:** the bodies a profile is declared for; an empty list means body-agnostic. A bake always uses the body transform supplied at export time.

**`version`:** the definition's declared version. It is not a publication revision counter.

**Publication.** An approved profile publishes the definition bytes from the approved snapshot together with the reference round-trip report stored for that proposal. `InterfaceProfileManifest` pins both as `{ name, sha256, bytes }` artifacts and names the approval's proposal, snapshot version and snapshot hash, so the definition and report bind to the same approval. The manifest lives at `data/interfaces/<sha256-of-manifest>/manifest.json` with `definition.yaml` and `report.json` beside it; a replacement approval writes a new directory, and the older definition, report and manifest path stay resolvable. A definition shipped with the baseline publishes with bootstrap provenance and a freshly generated reference report, not an invented human approval. Pending or rejected definitions are never published, and a missing or unreadable stored report fails publication instead of publishing an unbound profile.

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

**Validation (required before export):** evaluate the artifact through a software model of the declared pipeline on the probe set and report ΔE vs. target (median, p95, max, plus the per-CCT results above). Exports with p95 > 1.0 ΔE outside unsupported regions are flagged. `looklab export` refuses a flagged artifact and writes no file; `--force` writes it and still reports the flag. A Look that no declared grid size resolves within the threshold is not exportable through that profile: the exporter never exceeds the declared sizes or relaxes the threshold. The artifact bytes, thresholds and pipeline model do not depend on scope.

**Export pins.** Validation and download metadata name the selection scope and only the dependencies the pipeline consumes. The browser ZIP contains `export.json`, the verified `profile.yaml`, its `reference-report.json` and the output artifacts. Metadata pins the target version and manifest, profile manifest and definition/report hashes, export engine, format, bake illuminants and CCT, target-specific validation and artifact hashes. Camera-RGB insertion also pins the body's published input-transform prior by content address; replay requires that exact pin and never substitutes the current prior. Fixed scene-space profiles consume no input-transform prior. An installed prior is not measured calibration, and no path acquires a camera realization or control-response dependency it does not use.

**Reference report.** A submitted profile and a baseline-shipped profile use the same bounded reference check: the colourimetric Look, an identity camera transform, the probe set and the declared illuminant handling, capped at 2,000,000 LUT nodes. This checks the profile's declared pipeline; it is not a camera realization, fit or empirical qualification.

**Writers:** `.cube` (with DOMAIN_MIN/MAX; shaper as a separate 1D file where needed, variant documented per profile), HaldCLUT (16-bit PNG), `native_json`.

Later: darktable `.dtstyle` (darktable as a software body) and RawTherapee profiles.

## 8. Firmware card format

A card file carries one Look version for hacked camera firmware. looklab defines the file and its semantics; it does not model any camera's internal pipeline. The firmware translates the file into whatever its pipeline lets it set (a native film-sim stage, a LUT stage, or both), so the only information lost is what that firmware can't set. One file serves every camera that implements this section.

The file carries the family target, not a body realization (§8.1). Its META section carries a status: `qualified` when the target's published support mask supports at least one probe, and `experimental` when it supports none. An experimental card states that no region has an independently checked measurement yet; the surrounding page and download metadata say so, and firmware reads the same bytes under the same §8.4 tag. A card download requires a publishable valid target and its card, not a looklab camera realization, control-response model or their convergence; when the target publishes a support mask, a qualified card needs a supported probe in that mask, while a source-limited target with no supported probe gets a card marked experimental. Software export stays off for that target because export validation needs a supported probe. Firmware translation is verified separately through render tags and per-firmware scores (§8.4), so download metadata can say target evidence and firmware translation are unverified without inventing a pinned realization.

Every published Look version that offers a card includes `card.llk` as a manifest-listed artifact ([03 §5](03-data-model.md)). The writer is deterministic: the same Look version yields the same bytes.

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

Each section-table entry is `u32` kind, `u32` offset (4-byte aligned, inside the file), `u32` length, `u32` CRC-32 of the section's bytes. Entries are sorted by kind, with no duplicates and no overlaps. Readers ignore kinds they don't know. The manifest's SHA-256 covers the whole file; the content id lets firmware name the file in the render tag (§8.4) without hashing it. The reference writer lays payloads out in kind order, 4-byte aligned with zero padding before each section; the content id covers that padding. Readers accept any non-overlapping layout whose entries are sorted and aligned.

### 8.3 Sections

| Kind | Name | Required | Content |
|---|---|---|---|
| 1 | `META` | yes | TLV records `u8` key, `u16` length, bytes. Keys: 1 slug (ASCII ≤ 63), 2 version (ASCII ≤ 31), 3 name (UTF-8 ≤ 63 bytes), 4 short name (ASCII ≤ 27), 5 description (ASCII ≤ 511), 6 license and attribution (UTF-8 ≤ 255 bytes), 7 status (ASCII `qualified` or `experimental`; absent means `qualified`). Keys 1-4 are required; unknown keys are skipped. |
| 2 | `SEMANTICS` | yes | `u16` fields: input space (1 = §8.1), output space (1 = §8.1), shaper (1 = §8.1), viewing conditions (1 = reference), `wb_policy` (1 = adaptive), then `u16` = 0 padding. v1 readers reject any other value. |
| 3 | `NEUTRAL` | yes | `u32` n = 257, then n × 3 `f32`: the Look's output for a neutral input at `s = −12 + 18·i/(n − 1)`. This is the full Look on the neutral axis (T, N and the `C = 0` plane of Λ), evaluated exactly. It maps directly onto a native tone curve. |
| 4 | `FIT` | yes | `u32` n, then n records of 40 bytes: input `f32×3`, output `f32×3`, `σ_ΔE` `f32`, weight `f32`, flags `u32` (bit 0 = supported), probe index `u32`. v1 records are the probe set of the Look version, in probe-set order. Weight is the probe's `weight` from the probe set for supported probes and 0 otherwise, normalized to sum 1. |
| 5 | `GRID` | no | `u32` side (33 or 65), then side³ × 3 `f16`, red fastest, over the shaped input coordinate. Values are §8.1 output. A 65³ grid is about 1.6 MB; the other sections together are under 50 KB. |

**Grid interpolation (normative):** clamp each shaped coordinate to [0, 1], split it into a base node and fractions. Sort the fractions descending, ties broken red before green before blue, to get `f₁ ≥ f₂ ≥ f₃` with unit steps `e₁, e₂, e₃`. Then `out = (1 − f₁)·V₀ + (f₁ − f₂)·V(e₁) + (f₂ − f₃)·V(e₁ + e₂) + f₃·V(e₁ + e₂ + e₃)`.

**Section details (v1):**
- **Identity:** META slug is `[a-z0-9._-]{1,63}` and version is `[0-9A-Za-z.+-]{1,31}`, both ASCII; name and short name are non-empty, and the writer rejects bytes outside these grammars.
- **Status:** a META status of `experimental` is written only for a native-family target whose published support mask supports no probe; a `qualified` status is written for every other card, and absent means `qualified`. A reader rejects any other status value.
- **FIT record:** `input` is the probe's scene XYZ as linear Rec.2020, and `output` is pre-clip linear Rec.2020 (the same space as NEUTRAL). `σ_ΔE` is the Look version's published post-clip σ (docs/02 §5) and the supported flag is its published support mask; neither comes from a gamut test. A probe without published support is unsupported, with `σ_ΔE` = `+∞` (`0x7f800000`) and weight 0. `n` is the record count: readers accept any `n ≥ 1` whose byte length matches, and v1 writers emit one record per probe, in probe-set order. Weights are normalized over supported probes; when none is supported every weight is 0.
- **GRID values:** a node whose Look output channel is not finite is written as 0, and a finite channel beyond the f16 range clamps to ±65504. Nodes are red fastest: `r + side·(g + side·b)`.

**Reader rules:** reject the file unless the magic, version, lengths, header CRC and the CRCs of every section it reads match; every required section is present; counts match lengths; and every value it reads is finite, except a FIT `σ_ΔE` of `+∞` that marks unknown uncertainty. Unknown kinds are skipped after their CRC check; a duplicate kind is a mismatch whether or not the reader understands it. A rejected file changes nothing: the firmware keeps its previous state and never uses a partly read file. Sections the firmware doesn't use (typically `GRID`) need not be read.

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
`data/card/conformance-v1/` holds valid and malformed files with the expected reader outcome, grid-evaluation vectors (shaped input → output under the normative interpolation), and render-tag strings with their parse result. Valid files cover a qualified card and an experimental card. Grid outputs must match the reference within 2 f16 ulp *(tentative)*. Regenerate the set with `cargo run -p looklab-export --example gen_card_conformance -- data/card/conformance-v1`; `README.md` there describes every file.

## 9. Reference implementation

The reference consumer is looklab's own card reader and grid evaluator in `looklab-export`, native and wasm32, exposed as `looklab-cli card inspect | eval`, plus the conformance vectors of §8.6. An example translator in portable C99 with no allocator (`tools/card-translator-c99`) fits a 3×3 matrix and a per-channel tone curve from `FIT` and `NEUTRAL`, for firmware authors to start from. Software interface profiles keep their own bake → simulate → compare round-trip test in `data/interfaces/` with fixtures.
