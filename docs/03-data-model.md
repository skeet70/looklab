# 03 — Data model

## 1. Entities

```
Body ─┬─ FirmwareVersion ──(epoch map, versioned)── Epoch ─┬─ ControlSchema
      │                                                    ├─ NativeSim ──(realizes)── Look (shared family)
      │                                                    └─ ControlResponseModel (per body, epoch, base sim)
      ├─ InputTransform (shared calibration, versioned, dual-illuminant)
      └─ BodyRealizationParams (ξ, ω; versioned)

Look ── LookVersion (target: status, artifact, metrics, provenance, gate results; lut_defined frozen)
ModelSubject (native-family fitting) = SharedFamily(family) | BodyRealization(body, epoch, native_sim)
RealizationRevision (subject, family look@version, transform/params/CRM/clip, artifacts, status)
MatchBundleRevision (body, epoch, shared schema/transform/params, ordered sims, manifest)
TargetEligibility (immutable public snapshot: target look@version, basis, artifacts/base-pin, support/uncertainty, id)
CurrentTargetEligibility (derived current status)
ModelReport (immutable: subject, candidate, dependencies, capability, scope, outcome, limitations)
ReportDependency ── CurrentProjection (subject, use, selected report/eligibility/bundle set, applicability)

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
YAML, versioned per epoch. The server store is the authority; `data/bodies/` holds exported snapshots. A body's control schema is `data/bodies/<slug>.yaml`; its Body entity is `data/bodies/<slug>.body.yaml`. An approved mapping proposal inserts or replaces its `native_sims` entry in the epoch it names, and the updated schema is published in the next snapshot. A registered body without a schema receives its first schema through a `SchemaChange` proposal carrying the complete validated document; the body entity must already be registered in the shipped `data/bodies` or the previous snapshot. Example (the makernote tags and values are **placeholders** that must be verified against fixtures):

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

- This catalog is guidance, not the server-authoritative `ControlSchema`. Neither owner-reported nor confirmed state is read by schema proposals, fixture-backed makernote mappings, upload eligibility or any model path; mappings and schemas require the proposal and approval flow.
- `ManualConfirmation` version 1 carries the camera id and definition digest, reported firmware, complete or selected-control scope, and up to 64 observations. Each observation binds a control definition digest and either matches the documented options or supplies corrected availability and a typed domain.
- Agreement uses the latest claim per independent contributor identity group, control definition and reported firmware. Three eligible groups agreeing on the same actual options confirm that firmware's control. Different firmware versions are evaluated separately.
- Checks from profiles with no non-withdrawn serial-bearing photo of that camera count immediately, one group per profile, at the same control definition and firmware. When they agree and none conflict, the control shows `Owner-reported` with the reported options and the profile count in words. Owner reports never make a control confirmed.
- Reports that disagree at one control and firmware show `Owners disagree`, list every option with its count, and open one review item for that control and firmware. No option takes a majority. A moderator decision overrides owner reports for its firmware scope. Agreeing reports create no review item.
- Promotion to `Confirmed` keeps its audit-log entry. `Owner-reported` is derived display and writes none.
- A camera-level confirmation requires confirmed controls with a common explicit firmware scope. Unspecified or range-scoped moderator evidence is displayed with its scope, not as an all-firmware confirmation.
- Audited moderator decisions bind the current control definition and physical or video evidence. Confirmations grant no contributor tier, reputation, calibration support or scientific weight.
- Withdrawal excludes the owner's claims from agreement and requests publication. Existing sends are fenced against restoring those camera claims; a new approval can submit a new claim. Signed records and audit history are retained.
- Every active check is reported per control as intake, separately from agreement: checks against the current control definition, the subset from profiles with no non-withdrawn serial-bearing photo of that camera, and checks against an older definition. A profile's newest answer for a control and firmware replaces its earlier ones. When a reporting profile later contributes a serial-bearing photo of that camera, its newest answer moves into the linked count under the same replacement rule. Only linked identity groups count toward confirmation.
- The response to a signed confirmation carries a receipt for its sender: the camera, whether the profile is linked to it, and the number of checks recorded. Public summaries and pages do not carry it.
- The moderator queue lists each profile's newest active check with its firmware, finding, linkage and one disposition: counted toward agreement, part of a disagreement, recorded against an older definition, or reviewed. A disagreement is one queue item per control and firmware, listing every reported option with its count; no option takes a majority. A check is reviewed when it was received before a moderator decision on its control that is unscoped or whose firmware scope covers the check's firmware. Each decision's audit row records its decision id and the highest submission id present when it committed; a check submitted later remains part of a disagreement until a decision with a high enough watermark covers it, and a decision without that record reviews nothing. A check that arrives while a moderator is deciding is covered by that decision.

### Look and LookVersion
- `Look`: `id`, `slug`, `name`, `kind` (`native_family` | `lut_defined`), `wb_policy`, `description`, `aliases[]`, `license` (for `lut_defined`: the submitter's declaration + attribution).
- A `native_family` Look is the canonical shared family target, with one stable `id` and immutable coefficient versions. Camera calibration, realization parameters, CRM and clip are separate `RealizationRevision` records bound to an exact `look@version`; they are not target coefficients. A camera-local diagnostic fit can never become a target through latest-version selection. A family with no target may be seeded once from eligible native evidence on any supported body under existing trust, resource and gauge policy; that creates one provisional version without `RootAdmission` and records its source limitations ([02 §4.3](02-color-pipeline.md)). No camera allowlist gates intake or experimental realization publication.
- A `lut_defined` Look's identity is the LUT bytes, its declared encoding and its base. It also stores the grid, license and attribution, and a layered approximation fitted to the composition (§3, [02 §3.8](02-color-pipeline.md)).
- A `lut_defined` target is eligible for qualified matching through `TargetEligibility` (`basis: lut_composition`): existing import validity, frozen approximation, base-pin validity, inherited support/uncertainty and current base applicability. It requires no `native_prediction` report or native-camera admission. Destination and source camera realizations are assessed normally.
- `encoding` is `scene_referred { transfer, gamut }` for a LUT that consumes scene-referred log (v1: `transfer = flog2`, `gamut = fgamut`), or `display_referred { base, status }` for a LUT that expects an already-rendered image. `output` is `srgb` in v1.
- A display-referred `base` is `look@version` (a published Look; the neutral baseline `looklab-neutral-srgb-v1@1.0.0` is itself a fixed Look), `body_realization { body, renderer, film_simulation, dynamic_range, notes }` for an in-camera or software rendering, where `renderer` is `x_raw_studio` or `unspecified` when the donor named no engine, or `family_rendering { maker, rendering, dynamic_range, notes }` for an intended family Look that is not published yet. The `body_realization` base kind is a declared rendering, not a `ModelSubject::BodyRealization` and not a pinned model revision. A family base resolves to a `look@version` pin when that family Look is published; until then it is `base_identified_unmeasured`. A family declaration that names a setting the native release does not carry, such as `dynamic_range`, stays `base_identified_unmeasured` rather than pinning a settings-losing base.
- `status` is `pinned` (published and fitted), `base_identified_unmeasured` (fully declared and consistent, not yet measured or published), `base_incomplete { missing }` (a declared field is absent: the body, film simulation, renderer or dynamic range, reported in that order), or `base_inconsistent { field }` (the declared setting does not exist on the declared body; `field` names it). Only `pinned` is matchable and gets a card file; the other three are stored and listed with an approximate preview at most.
- `LookVersion`: `version`, `status` (`provisional` | `rooted` | `published` | `deprecated`), `artifact_hash`, `probe_predictions_hash`, `metrics`, `provenance` (§3), `gate_results`, `changelog`. `status` records lineage, root state and the lifecycle label, not publication eligibility or empirical qualification by itself; a candidate is served only when it also passes the numerical and publication gates and has an applicable current projection ([05 §8.1](05-trust-and-integrity.md#81-scoped-qualification)). Provenance records the pinned base; a new base version creates a candidate rebased version gated against the Look's anchor, and a base revert marks every pinned dependent ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)). Each published `lut_defined` version stores its declared base in `lut_base_pins`; a pin that names its own Look or closes a Look dependency cycle is rejected. Publishing a newer version of a base Look, resolving a pending family base to a native `look@version`, or revoking a root admission the base used writes a `lut_rebase_pending` row, and the worker consumes those rows in a `lut_rebase` job independently of image pools. A candidate publishes only after its gate passes; a held candidate leaves the previous version's bytes and matchability unchanged, and a pin is never re-anchored automatically. Publishing a new family version queues dependent realization work against that candidate; existing realization revisions stay pinned to their original family version and are not retargeted.
- Family identity and lineage persist across cohort changes. Admission of another body, realization publication, epoch/sim scope splits and later qualification create a new family candidate or report association; they do not reset the family anchor, gated union, support history or gauge identity, and they do not mutate old manifests, cards, bundles or realization artifacts ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)). A `lut_defined` target keeps its frozen composition and is never trained by native-family admission ([02 §3.8](02-color-pipeline.md)).
- Same name ≠ same Look. Cross-body disagreement can propose a split.
- Rendering classification is orthogonal to `Look.kind`: a native-family or LUT-defined Look may be monochrome. A future monochrome response retains the common J'a'b' interface, with lightness and potentially tone-dependent tint in both chroma channels. Its model is undecided; a camera-RGB mixer followed by a tone curve is only a candidate.

### InputTransform
`body_id`, `version`, `A0_A`, `A0_D65`, `A0_source` (`ssf` | `dng` | `db` | `chart`), `delta`, `covariance` (6×6), `anchor_count`, `created_at`.

Shared calibration for the body: one transform version serves every realization on that body. `A0_source` is an approved, pinned server-side profile (SSF, DNG, database matrix or chart anchor) with persisted source identity and uncertainty; a local decoder matrix is not a server profile and the upload format carries none. A missing approved source blocks the computations that require server-authoritative calibration and reports the specific fitting blocker; it is not an identity transform. Intake and preparation proceed when their own inputs are available, and a local decoder profile may enable preparation without establishing server authority. The fit evaluates `A_c(T_i)` per image by interpolating both matrices at the server-recomputed CCT; a D65-only matrix is not substituted. A `RealizationRevision` pins this version.

### BodyRealizationParams
`body_id`, `version`, `xi` (per exposure mode), `omega` (per WB mode class), with covariances.

Body-level and shared by every realization on the body; a `RealizationRevision` pins this version. `clip_k` is not stored here: it is keyed by `(body, epoch, native_sim)` and recorded on the `RealizationRevision`.

### ControlResponseModel
`body_id`, `epoch`, `base_sim`, `version`, `D` coefficients, `support` (controls/ranges with data), `metrics`.

Scope is the realization subject `(body, epoch, base_sim)` and the exact family version it was fitted against. Two sims mapped to one family, or two epochs on one body, do not share a CRM. Missing control values are unknown, not default. A CRM is a dependency of a `RealizationRevision`, not a target artifact.

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

### ModelSubject, RealizationRevision and qualification records
- `ModelSubject` is a closed typed identity for native-family fitting: `SharedFamily { family }` names the canonical `Look` and its stable family id; `BodyRealization { body, epoch, native_sim }` names one camera realization scope. A `lut_defined` Look's target identity is its LUT bytes, declared encoding and base, and it is not a `ModelSubject`. Two epochs on one body, or two sims mapped to one family, are different subjects. No body-wide boolean substitutes for the subject. Family ids are stable across cohort changes; firmware and effective-contributor identities are resolved consistently within one invocation, and body-local ordinals are not global firmware identities.
- `RealizationRevision`: `id` (globally unique immutable revision id), `subject` (`BodyRealization`), `family_look_version` (`look@version`), `input_transform_version`, `realization_params_version`, `control_response_version`, `clip_k`, `artifact_ids`, `status` (`candidate` | `published` | `deprecated`), `created_at`. Its canonical token is `realization:<id>`. It pins exact dependency versions and is not a target Look. A new revision changes realization state without redefining the family target; old revision ids stay resolvable and their bytes immutable. `status` records lifecycle state only; numerical validity, publication eligibility and empirical qualification are separate axes ([05 §8.1](05-trust-and-integrity.md#81-scoped-qualification)).
- Saved camera calibration, native/published warm states, frozen body pins and fit checkpoints pair numeric exposure/WB classes with their originating `ExposureModeKey`/`WbModeKey` inventory. Restoration resolves coordinates by key; class ordinals are not identities across epoch or realization subsets. Each checkpoint carries its own inventory. Missing legacy mappings are unavailable, not unchanged ordinals or identity corrections.
- A component gauge carries its designated body's inventory. A fixed-physics conditional fit can retain that external reference without the reference body's observations. Physical WB modes observed in metadata do not require a schema enum control; a fit that creates shared calibration requires a resolvable observed WB mode. State rebinding does not change absolute Look coefficients or immutable historical bundle bytes.
- A qualified family's cached input identity includes the cohort report, every scoped member admission and the effective sample-admission masks. Adding an admitted member changes that identity even when no image is added; exclusively experimental evidence does not. Common lineage/reference selection uses members contributing weighted eligible native evidence. A fixed-physics conditional fit restores its own saved reference state even when it adds only control evidence.
- `TargetEligibility`: immutable public proof summary with `id` (stable content digest over `target_look_version`, `basis`, sorted `artifact_ids`, `base_pin`, `support_uncertainty_source` and sorted `dependency_ids`), `target_look_version`, `basis` (`native_family` | `lut_composition`), `artifact_ids`, `base_pin`, `support_uncertainty_source`, `dependency_ids`, `reasons`, `created_at`. Its canonical token is `eligibility:<id>`. A `lut_composition` basis derives from the existing import validity, frozen approximation, base-pin validity and inherited support/uncertainty plus the current applicability of its base; it is not a `ModelReport` and does not require `native_prediction` or native-camera admission on the LUT. A `native_family` basis is grounded in the pinned family version's published mean-response, probe, support and uncertainty artifacts plus the applicable source/admission evidence under its source limits. Publication eligibility follows those artifacts and the base pin, so a source-limited target with no supported probe still carries an `Eligible` snapshot while its selection scope is `Experimental`; its version status follows the existing publication path, `published` for a published target and `provisional` for a seed. It does not depend on a derived `TargetEligibility` entry inside a `SharedFamily` current projection, and it does not require a `BodyRealization` native-prediction report on the family. A source-limited target is usable for the same-camera qualified loop before P1.9; cross-body claims require their separate evidence. The snapshot is created on publication and reused when the tuple is unchanged; a historical permalink pins `eligibility:<id>` and resolves this snapshot.
- `CurrentTargetEligibility`: `eligibility_id`, `status` (`eligible` | `ineligible` | `invalidated`), `reasons`, `applicability`. It is the derived current status of a `TargetEligibility` snapshot, recomputed when an input artifact, base pin or base applicability changes. It is not an immutable report and does not rewrite the snapshot id. Destination and source camera realizations remain `BodyRealization` subjects assessed normally.
- A family with no target may be seeded once from eligible native evidence on any supported body under existing trust, resource and gauge policy. Seeding creates one provisional `LookVersion`, records source limitations, grants no `RootAdmission`, and fences concurrent initial selection. The seed/reference cohort may refit that target as eligible data arrives under unchanged gates; seed membership is source-limited and is not cross-body qualification. A new body fits against the pinned version and joins the qualified cohort only through scoped P1.9 agreement and the applicable engine prerequisite ([02 §5.1](02-color-pipeline.md)). Independent camera-local fits are diagnostics, not targets.
- `ModelReport` is immutable: `id` (opaque stable report id; canonical token `report:<id>`), `subject` (a `ModelSubject`, so `SharedFamily` or `BodyRealization`), `candidate` (family version or realization revision), `artifact_ids`, `capability` (`native_prediction` | `controls_matching` | `cross_body_admission`), `scope` (settings and combinations, output encoding, colour/exposure cells, epoch/sim, assessed domain, `ScoringMetric` and `OutputContext`), `rule_revision`, `fitter_evaluator_model_identity`, `probe_set`, `effective_options`, `partition_policy`, `assessment_seeds`, `evidence_membership` (private blob and report references, known groups and training/assessment roles, plus the actual evaluated image and contribution set after protocol filtering), `dependency_ids`, `outcome` (`pass` | `insufficient_evidence` | `fail` | `execution_failure`), `metrics`, `supported_mask`, `limitations`, `evidence_requests`. A `lut_defined` target assessment is `TargetEligibility`, not a `ModelReport`. The stored record keeps private evidence membership and dependency ids; `PublishedReportSummary` is the whitelisted published subset, never private blob references, sample fingerprints, group identities, RAW hashes, coordinates, credential keys or serial HMACs. An outcome is not publication permission. Applicability is not a report field; the current projection carries it ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)).
- `ReportDependency`: `report_id`, `kind`, `target` (contribution, image, root admission, transform, profile, schema, family version, realization revision, bundle revision, report, cohort revision, gauge/component revision, identity-resolution or weighting revision, or rule version), `required`. Links are queryable rows, not only a digest, so withdrawal, exclusion, identity merge, root revocation, report supersession or reinterpretation can find every dependent assertion. Execution timestamps and job attempts do not change semantic report identity.
- `MatchBundleRevision`: `id` (lowercase hex SHA-256 of the exact canonical manifest bytes; canonical token `bundle:<id>`), `body`, `epoch`, `schema_snapshot_version`, `input_transform_version`, `body_realization_params_version`, `sims[]` ordered source entries `{ native_sim, realization_revision_id, family_look_version, control_response_version, base_artifact_id, crm_artifact_id, covariance_artifact_id? }`, `excluded[]` entries `{ native_sim, reason }`, `manifest_hash`, `created_at`, `producer`. The published manifest omits `id` and `manifest_hash`; both record fields equal the digest of those exact bytes and are stored outside them in the registry, authenticated index and bundle locator. It is target-independent: the target `look@version` is selected separately by the request or permalink. All sims in one revision share the pinned schema, transform and body parameters; an incompatible sibling revision is excluded with a typed reason. Source entries carry artifact and version references only; qualification is not baked into immutable bundle bytes. A camera-only realization update publishes a new bundle revision and does not create or rewrite a target Look release. Old target-owned bundles and manifests remain historical, immutable and resolvable. New report qualification reuses the same bundle bytes through an association.
- `CurrentProjection`: `subject` (a `ModelSubject` for native-family claims, a published target `look@version` for `lut_defined` target eligibility, or a `BundleScope(body, epoch)` for bundle selection), `use` (`qualified_match` | `experimental_discovery` | `preview`), `selected[]` (small typed entries `{ capability, scope_key, report_id?, eligibility_id?, bundle_id?, artifact_ids }`, where `capability` is `native_prediction` | `controls_matching` | `cross_body_admission` | `target_eligibility` | `bundle`, and `scope_key` is settings/combinations, output encoding, colour/exposure cells, epoch/sim, `ScoringMetric` and `OutputContext`), `pending_generation`, `applicability`, `invalidated_reason`. Each entry selects one applicable scoped report, `TargetEligibility` or `MatchBundleRevision` for a capability and scope key; a new report or bundle revision supersedes only entries with the same capability and overlapping scope, so native prediction, controls/matching, cross-body admission, LUT target eligibility and bundle availability remain independent. For a `lut_defined` target, `subject` is the target `look@version`, `capability` is `target_eligibility`, and the entry uses `eligibility_id` naming the published `TargetEligibility` snapshot; its current status is the separate `CurrentTargetEligibility`, not a synthetic native-prediction report. For a bundle scope, `capability` is `bundle`, `bundle_id` names the selected revision for that `use`, and the target Look version is not part of the selection. Bundle availability is not a qualification label: a qualified selector composes the selected current report set and uses only the settings and sims it covers, while other bundled sources never enter implicitly. Qualified and experimental selectors keep separate compatible last-good bundle snapshots per use, so a new experimental calibration snapshot cannot hide an older still-valid qualified bundle. Historical permalinks pin immutable report, eligibility and bundle revision ids and the full ranked bundle eligibility at that time; current applicability is checked separately through this projection and never rewrites the pinned ids. The projection is small, transactionally updated, published as a hash-bound snapshot, and checks selected-predecessor identity so an older run cannot roll back a newer selection; it is not a policy platform or immutable event stream. A failed replacement preserves its valid predecessor. Withdrawal, exclusion, identity merge, root revocation or reinterpretation invalidates affected current projections immediately, even when the fitting pool is empty, and fences affected in-flight commits; append-only arrivals queue a successor without rejecting a still-valid snapshot. Reports and projections use the existing `Job` table and publication request; there is no separate scheduler or status store ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)).
- Canonical reference tokens: `look@version` (each component percent-encoded before joining), `realization:<RealizationRevision.id>`, `report:<ModelReport.id>`, `eligibility:<TargetEligibility.id>`, `bundle:<MatchBundleRevision.id>`. IDs are typed opaque immutable identities resolved and hash-verified through their manifest or public summary snapshot; a mutable version ordinal is never a realization identity. A v3 `reports` entry is `subject=<ref>`; empty `reports` is valid only for an experimental unassessed context. v1, v2 and v3 links remain historical unassessed under the pre-clip metric and are never silently converted or requalified.
- These records keep preparation availability, numerical validity, publication eligibility, empirical qualification and rooted trust separate; a pass in one axis does not imply another. Pending work is tracked separately from input invalidation. Implementation rationale for the subject boundary, report lifecycle and migration is in [model qualification plan §3](model-qualification-plan.md#3-model-subjects-and-artifact-identity) and [§8](model-qualification-plan.md#8-reports-dependencies-and-automatic-lifecycle).

Public eligibility snapshots name only public artifact, report and proof references. Private evidence membership and dependency rows remain private.

### Rendered metric, output context and assessment status

- `ScoringMetric` is `RenderedPostClipV1` or `LegacyPreClipV1`, serialized `rendered_post_clip_v1` / `legacy_pre_clip_v1`, with identity tokens `looklab-scoring-metric/rendered-post-clip/1` and `looklab-scoring-metric/legacy-pre-clip/1`. It is defined in `looklab_schema` and re-exported from `looklab_match`. A rendered score also carries `OutputContext`; a legacy score has none and keeps its historical pre-clip meaning. The metric identity and, when present, the output context identify the error definition; values from different identities are not comparable. `LegacyPreClipV1` records the historical estimand that compared a pre-clip target/prediction with the decoded post-clip JPEG; it does not assert that historical observations were inverse-clipped.
- `OutputPin { encoding: OutputEncoding, clip_k: f64 }` and `OutputContext { target: OutputPin, camera: OutputPin }` are defined in `looklab_model` and re-exported from `looklab_match`; there is one canonical definition of each. `OutputContext` has canonical string `target=<encoding>@<clip_k>;camera=<encoding>@<clip_k>`. The target pin comes from the target version's pinned reference encoding and clip; the camera pin comes from the selected output encoding and the `RealizationRevision`'s clip. The target and camera render at one fixed calibrated scene input `p`, and the captured observation is decoded once into the common CAM16-UCS frame ([02 §5](02-color-pipeline.md#5-uncertainty)). The context does not re-bin colours or change sample membership. The actual camera output context is part of the score and residual-observation grouping identity, even when two recipes share the same controls and settings. Output encoding is not assumed to be a schema control.
- `TargetAppearance { PreClip, Rendered(OutputPin) }` is the required target input to a match or score. `Rendered(pin)` selects the post-clip target; `PreClip` is the historical diagnostic form. The `ScoringMetric` is derived from that input and is never defaulted.
- `AssessmentStatus` is `Assessed`, `Partial` or `Unassessed`; it is defined in `looklab_schema` and re-exported from `looklab_match`. `Partial` reports the covered subset of the declared assessment set and never hides unsupported points from the denominator. `Unassessed` records absent evidence, not a failure and not permission to substitute a nearby probe's support or uncertainty. `DeltaStats` carries `AssessmentStatus::Unassessed` for a descriptive captured error with no established joint uncertainty or significance. A residual or error metric value without an assessment status is legacy and unassessed.
- `SettingsCard.sigma_band` is the mean-model marginal band only. `SettingsCard.joint_uncertainty` is `AssessmentStatus::Unassessed` in the current core because no cross-covariance is available; it is not a rendered joint uncertainty estimate.
- `RecipeScore` records `metric`, `output?`, `recipe`, `predicted_de`, `predicted_sigma`, `measured_de`, `measured_sigma`, `raw_gap`, `signed_residual` (observed minus predicted, 3-vector), `n_verifications`, `model_versions`, and `source`. `source` pins the exact `RealizationRevision`, the selected target `look@version` or `eligibility:<id>`, and the `BodyStateVersion` the prediction was computed from. A rendered score's two ΔE values use the same `OutputContext`; a legacy score carries `metric = LegacyPreClipV1` and no output context. Historical scores are never silently reinterpreted or requalified, and a rendered score is not compared with a legacy score.
- `DeclaredCondition { colour: ColourCondition, weight: ConditionWeight }`; `ConditionWeight` is finite and greater than zero. `RegionConditionScope { cell: SceneCell, output: Option<OutputContext>, conditions: Vec<DeclaredCondition>, contributors, aggregate_sigma: Option<RegionalSigma> }` declares the fixed population for a regional aggregate. Duplicate or invalid scope inputs are rejected. A single colour does not stand for the whole Region unless the scope declares only that colour. Missing cross-colour pooled σ yields `AssessmentStatus::Unassessed` and no persistent claim. A two-colour scope with weights 1 and 3 has mean 2.5; without the producer joint σ it yields no bounty, while an explicit producer joint σ yields one Persistent regional bounty. Both `analyze_residuals` and `analyze_conditional_cells` take these scopes; an empty scope list is the honest missing-producer record.
- `ResidualObservation` records `metric`, `output` (`OutputContext`), `source`, `cell` (`SceneCell::Region`, `SceneCell::Neutral { exposure }` or `SceneCell::OutOfDomain`), `settings`, `raw_gap`, `signed_residual`, `prediction_residual`, `observation_sigma`, `joint_covariance`, `assessment`, `reachable` and `attribution`. `RegionResidual` is one per regional context and records `output`, `cell`, `colour`, `contributors`, `settings_cells`, `raw_gap`, `observation_sigma`, `systematic_excess`, `systematic_sigma`, `assessment`, `reachable`, `persistent`, `attribution` and `alignment`. `colour` is `Some` only when the scope declares exactly one condition and `None` for a multi-condition scope. `systematic_sigma` is `None` unless the full regional assessment is `Assessed`, which requires per-condition joint moments and the declared aggregate σ; `region_uncertainties` never falls back to `observation_sigma`, because observation scatter is not model-mean or regional-gap uncertainty. Both group by the actual `OutputContext`, so identical controls with different camera output do not merge. `attribution` is one of `input_transform`, `control_model`, `look`, `mixed` or `unassessed`; `assessment` is `AssessmentStatus`, and `reachable` is a `ReachabilityStatus`. `SceneCell::Neutral { exposure }` is a category, not hue zero; it is separate from the global 12 hue × 6 exposure trust-cap cells and changes no cap or rate policy.
- `RecipeLeaderboard` records `metric` and `output` and ranks only scores with the same identity.
- A `ModelReport` metric value carries its `ScoringMetric` and, for a rendered metric, its `OutputContext`. A report that assessed a legacy metric keeps that identity in `metrics` and in its scope; later reports over unchanged bytes do not rewrite it.
- The rendered metric identity is a scoring and report identity. It does not serialize Look coefficients, change card bytes or add upload fields; a historical artifact or link keeps the metric meaning it recorded.

### Credential
`id` (random internal id), `webauthn_credential` (credential id + public key, as `webauthn-rs` stores it), `display_handle?` (optional, user-chosen), `tier` (`new` | `endorsed` | `seeder`), `roles` (`moderator`, `admin`), `reputation`, `created_at`, `status` (`active` | `revoked`). Status tokens are hashed rows in `status_tokens`, one per signed-in browser (capability-token status feed; [06 §4](06-privacy-and-payload.md)). Public keys and serial HMACs are never published.

### Grant, Endorsement, RootAdmission
- `Grant`: `id`, `subject_credential`, `role_or_tier` (`seeder` | `moderator` | `admin`), `scope?: { rules: [{ body, look }] }` where either side may be `*`, `granted_by` (an admin credential or `cli`), `reason`, `granted_at`, `revoked_at?`.
- `Endorsement`: `id`, `issuer_credential` (a seeder), `subject_credential`, `scope: { rules: [{ body, look }] }`, `reason`, `evidence_refs[]` (e.g. a reviewed chart session), `issued_at`, `revoked_at?`.
- `RootAdmission`: `id`, `admitted_by` (a seeder), `contribution_id`, `scope: { rules: [{ body, look }] }`, `reason`, `admitted_at`, `revoked_at?`.
- Every create/revoke writes an audit-log entry. The lists are public by internal id or display handle.

### Contribution and ContributionImage
- `Contribution`: `id`, `credential_id`, `batch_manifest_hash`, `signature`, `pow`, `kind` (client-claimed: `library` | `rerender` | `pattern` | `gradient` | `chart`), `serial_hmac?` (server-computed; absent if the contributor opted out), (the effective contributor is **resolved at fit time** from current serial links, like epochs, and never stored on the contribution; [05 §2](05-trust-and-integrity.md)), `intent` (`native` | `recipe` | `card`), `target_look_id?`, `card?` (for intent `card`: firmware id and version, look version, content-id prefix, and whether the tag was present or user-declared; [04 §8.4](04-matching-and-export.md)), `body_id`, `firmware_version`, `sim_key`, `settings`, `status` (`pending` | `accepted` | `quarantined` | `withdrawn`), `created_at`. A row in `contribution_rejections` (`contribution_id`, `moderator_credential`, `reason`, `rejected_at`, append-only) makes `quarantined` permanent for that contribution; automated worker updates cannot leave quarantine, while withdrawal remains terminal.
- `ContributionImage`: `id`, `contribution_id`, `samples_hash`, `n_samples`, `meta` (sanitized), `rerender_group?` (client-assigned id, see §2.1), `setting_source` (per control: `reproduced` from the makernote through the named snapshot's schema, or `declared` by the contributor; [06 §1](06-privacy-and-payload.md), [07 §4](07-contribution.md)), `duplicate_of?`, `fit_stats` (`rms_de`, `z`, `reason?`), `weight_components`.
- Contributions made through signed sends store the send nonce. Status groups these records as one submission; records without a send nonce are grouped by UTC day and camera as earlier activity. Status responses contain `activities[]` with nested `contributions[]`. RSS identities are stable when a submission's members change and are scoped to the credential.
- Every contribution signs `render_class` (`colour` | `monochrome`) independently of `kind` and `intent`. Deferred monochrome also signs its explicit model-development authorization. A monochrome card render has intent `card`.
- Deferred pool records retain rendering firmware, schema snapshot, original native sim, normalized family and settings with their sources, authorization time and a fixed 180-day expiry. Their scientific weight is zero until monochrome analysis exists; they never enter colour fit, calibration, gauge, support, release scores or reputation.

### Proposal and ModerationDecision
- `Proposal`: `id`, `credential_id`, `type` (`body` | `schema_change` | `mapping` | `interface_profile` | `lut_look` | `epoch_split` | `look_split` | `gate_override`), `payload` (validated YAML/JSON), `human_summary` (a menu-level description, e.g. "adds Reala Ace to X-T5 firmware 2.0+"), `evidence[]` (confirmations carrying parsed whitelisted tag values; fixture donations; worker evidence), `status`, `created_at`.
- `ModerationDecision`: `proposal_id`, `moderator_credential`, `decision`, `reason`, `decided_at`. Logged.
- **Moderation actions** beyond proposals (also logged): re-anchor a Look, revert to version N and reject contribution(s) C ([05 §8](05-trust-and-integrity.md)), block a credential's future intake (`credential_intake_blocks`: `credential_id`, `moderator_credential`, `reason`, `blocked_at`, append-only), handle serial-reuse and new-credential-burst flags, review endorsement nominations. A block denies new contributions only; it does not revoke the credential or change existing data, standing, tier or qualification.

### Recipe, RecipeScore, Bounty
- `Recipe`: `id`, `credential_id?`, `body_id`, `epoch`, `base_sim`, `settings`, `target_look_id`, `source`.
- `RecipeScore`: `metric`, `output?`, `recipe`, `predicted_de`, `predicted_sigma`, `measured_de`, `measured_sigma`, `raw_gap`, `signed_residual`, `n_verifications`, `model_versions`, `source`. The metric and context semantics are in [Rendered metric, output context and assessment status](#rendered-metric-output-context-and-assessment-status); a historical score keeps `metric = LegacyPreClipV1`.
- `Bounty`: `look_version_id`, `body_id?`, `region`, `metric`, `output?`, `assessment`, `reachability?`, `expected_variance_reduction`, `reason`, `status`. A bounty rests on an assessed systematic excess or an assessed coverage gap; an `Unassessed` or `Partial` region is not evidence of a persistent residual. A residual bounty carries a `Reachability` witness; a coverage-gap bounty need not. `Reachability { witness, tolerance, declared, evaluated, assessed, within_tolerance, within_assessed, status }` uses `ReachabilityStatus::{Reachable, Partial, NoRecipeFound, Unassessed}`, with `REACHABILITY_TOLERANCE_DE = 2.0`. `evaluated` and `within_tolerance` are descriptive rendered results; `assessed` and `within_assessed` count only points with an applicable target and realization/settings-domain witness, and `within_assessed` drives `status`. Without that domain, retain `declared` and all raw `evaluated`/`within_tolerance` counts, set `assessed = 0` and `within_assessed = 0`; a raw descriptive miss may still report `NoRecipeFound` (not unreachable), while all raw points within tolerance without domain report `Unassessed`. Only `within_assessed` can justify `Partial` or `Reachable` under applicable support. Missing target uncertainty or support leaves confidence `Unassessed`, not fully supported ([05 §12](05-trust-and-integrity.md#12-recipe-channel-and-bounties)).
- `EvidenceRequestWire`: public response containing `origin` (`report` with its immutable report id, or `pre_candidate`), typed model `subject`, `capability`, `kind`, `detail`, and optional `observed_groups`. Subject identity supplies the camera, epoch and native simulation. Private evidence membership and dependency rows are not included.

### AuditLogEntry, Job, CommunityDataSnapshot
- `AuditLogEntry`: `seq` (autoincrement), `type`, `payload` (JSON text), `ts`. Append-only (no UPDATE/DELETE in application code). Published as a page and JSON ([05 §11](05-trust-and-integrity.md)).
- `Job`: `id`, `kind`, `payload`, `status`, `attempts`, `run_after`, `locked_until`. The worker polls this table. Qualification assessment, report and current-projection updates, publication requests and evidence requests reuse this job table.
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
3. **Align.** Per-axis similarity (separate horizontal and vertical normalized scales), rotation, translation and radial distortion via multi-scale phase correlation, then LM refinement on normalized cross-correlation. Reject if central Spearman rank correlation of the projected intensity is < 0.98; a monotone tone curve between the two renderings does not lower it.
4. **Downsample** to ~1.5 MP (area average).
5. **Mask:** clipped RAW channels (≥ 0.98 of white), local gradient above the 90th percentile, 3% border, RAW pixels whose footprint falls outside the JPEG's field of view (an aspect-ratio crop), chart/pattern regions (handled separately).
6. **Density sampling** in `(s, h_in, √C_in)` using the body's prior transform at the image's CCT, where `s = log2(Y/0.18)` clamped to [−12, +6] and `(h_in, √C_in)` come from CAM16-UCS of the chromaticity scaled to Y = 0.18 ([02 §3](02-color-pipeline.md)). Up to N = 50,000 samples, inclusion probability ∝ 1/bin_count (capped). Store `density_w`, the inverse inclusion probability of the actual selection (1.0 when every candidate is selected); the fit consumes it as a sample weight.
7. **Re-render groups:** for JPEGs rendered in camera from the same RAW, sample **one shared pixel set** and apply **one shared shuffle**. The members' `in_rgb` columns are then identical. That gives the control-response fit exactly matched inputs, and lets a future server-side check confirm groups without any RAW hash.
8. **Shuffle.** No spatial information leaves.

**Sampling seed (privacy invariant).** Density sampling and the shuffle use a **fresh CSPRNG seed per image (or per re-render group)**. It's never derived from anything the server knows (batch nonce, ids, hashes), never uploaded and never stored server-side. Anyone who knew the seed could undo the shuffle and recover sampling order, which could be close to raster order. The client can export the seeds **locally** as a seed file, so users can reproduce the payload byte for byte ([06 §5](06-privacy-and-payload.md)). Seeds are **sensitive**: anyone holding a seed and the upload can recover the sampling order. The UI labels them as such and warns against pasting them into bug reports. Fixed seeds are allowed only in tests.

**Reproducibility.** The payload depends on more than color math: RAW/JPEG decode, FFT and LM alignment, the gain fit, CCT estimation and downsampling. Native builds pick SIMD paths at runtime, so a native CLI can't reproduce the browser's bytes. The reference prep is therefore the **`looklab-wasm` prep module**. The CLI's `verify-payload` command runs that module, built by the user from source, under wasmtime. Identical inputs, seed file and prior-transform version give identical bytes by construction.

Monochrome preparation keeps all three JPEG output channels, the declared output colour space, density weights, flags and shared re-render grouping. Mixed colour/Acros/Sepia groups reuse one RAW decode, input sample set and shuffle. Prep version 2 samples each JPEG through its own validated alignment at the shared RAW-selected points; version 1 reused the reference JPEG's mapping for every member. The sample container remains version 1. No source image or RAW hash is added to the payload.

Prep version 3 rejects explicit JPEG ICC profiles that bounded profile-name identification cannot recognize as sRGB or Adobe RGB. Numbered ICC chunks must form one complete profile; its declared size, `acsp` signature and RGB header must agree, and every nonempty description must identify the same supported space. This is not general ICC validation or colour-space conversion. With no ICC profile, the existing EXIF and missing-profile policy applies. The preparation version and module identity change; historical payload verification requires the matching historical CLI and prep module.

Prep version 4 aligns with a per-axis scale: the JPEG's normalized frame maps onto the RAW's with separate horizontal and vertical scales seeded by the two pixel grids, a rotation, a translation and radial distortion. An in-camera aspect-ratio crop (16:9, 1:1 and the like), a centred or off-centre crop and a reduced-size render are therefore one model. For equal-aspect pairs a same-aspect prior keeps the ratio at the dimension seed while the content can still move it. The overlap of every member's JPEG field is masked before density sampling. The central acceptance gate is the Spearman rank correlation of the projected intensity, so a monotone tone curve between the renderings does not lower it. The preparation version and module identity change; historical payload verification requires the matching historical CLI and prep module.

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
version          u16 = 4
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

The prep module advertises postcard ABI 2 and a build identity derived from that ABI, the preparation version, and the envelope and send-manifest versions. The page checks the module's version reply before its first operation and does not send that operation when the identities differ, asking the owner to reload and prepare again. The `ll_alloc`, `ll_free` and `ll_call` exports are unchanged.

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

For a canonical family target, the `look.json` + `look.bin` pair is the immutable target artifact: it carries `θ_L`, evaluator metadata and probe uncertainty. It does not embed camera calibration, realization parameters, CRM coefficients or a camera's clip. A per-sim base realization artifact uses the same evaluator format but is derived from a pinned family target and its `RealizationRevision`; it is not a separately promoted target. `provenance.bodies[]` records dependency and revision identities used to fit it, and those are references, not target parameters; its per-body `control_response_version` and `clip_k` summarize the contributing realization revisions, whose ids are recorded in `provenance.bodies[].realization_revisions`, while the authoritative keyed values live on those `RealizationRevision` records. A `BodyRealization` publishes as a separate realization artifact plus `RealizationRevision` bound to this exact `look@version`; changing it does not change this artifact or its hash.

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

A display-referred LUT replaces `encoding` with `{ "kind": "display_referred", "base": { "kind": "body_realization", "body": "fujifilm/x-e3", "renderer": "x_raw_studio", "film_simulation": "classic_negative", "dynamic_range": "dr400", "notes": "..." }, "status": { "kind": "base_inconsistent", "field": "film_simulation" } }`. `status.kind` is `pinned`, `base_identified_unmeasured`, `base_incomplete` or `base_inconsistent`; the last three are stored and listed but not matchable and have no card file. A `base_identified_unmeasured` family base becomes `pinned` in a new version when a native Look for that rendering is published, unless the declaration names a setting the native release does not carry; the rebased approximation is refitted against that `look@version` rather than the neutral preview. Provenance repeats the pinned base as `base`; a base publish or root revocation marks the version in `lut_rebase_pending` without rewriting its immutable file ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)).

`look.bin` is a named-array container: `LKBN`, a little-endian `u32` array count, then per array a `u8` name length, the ASCII name, a little-endian `u32` element count and that many little-endian `f64`, arrays sorted by name. v2 names are `T` (absent for a colorimetric tone), `N`, `G`, `R`, `Lambda`, `res`, and optionally `probe_cov` and `probe_cov_post` (9 `f64` per probe, row-major 3×3), `probe_sigma`, `probe_sigma_post`, `probe_bias`, `probe_pre_reliable` and `support` (one `f64` per probe), `kde` (one `f64` per probe) and `lut` (`size³ × 3` `f64`, red fastest, for a `lut_defined` Look); `res` is channel-minor, `((s·17 + a)·17 + b)·3 + channel`, and holds the 17³ cubic B-spline control points. Each `G`/`R`/`Lambda` array is 780 `f64`: 10 plane, 720 volume, then 50 near-neutral core coefficients indexed `[s][term]` for the five normalized Cartesian monomials of docs/02 §3.3. v2 fixes the knot vectors, residual domain and core join radius, repeated in `look.json` for validation; a stored LUT side is at most 64, checked before allocation; the zip is stored, fixed-order and fixed-timestamp. A `looklab-look/1` file is refused with a format error: its coefficient meaning has no exact v2 representation, so it must be refitted and republished under v2.

**Evaluator semantics (v2).** Evaluation follows [02 §3](02-color-pipeline.md): physical `s = log2(Y/0.18)` and the bounded spline coordinate `s_L = E(s)` (identity on `[−12, +6]`, C² transitions outside it with widths `S_SHADOW_TRANSITION = 2` and `S_HIGHLIGHT_TRANSITION = 1` stops, saturation at `−14` and `7`). The tone, G/R/Λ and residual use `s_L` and extrapolate their end cubic pieces outside the knot range; the colorimetric tone uses physical `s`. The tint continues C² above `J' = 100` to `J'_max = solve::neutral_j_prime_max()` and is constant above it. The near-neutral coordinates are `n = K·(a, b)/D` (pre-power cone-opponent coordinates of the input at `Y = 0.18`, `K = C0·D_ref/ρ0` from the reference viewing conditions). The core input is `n`. Two pre-clip chroma paths are evaluated: a CAM16-UCS path with the layers in UCS, and an internal path with the layers on the normalized opponent reference `N` of the reference stimulus. The published chroma is `w·UCS + (1 − w)·Φ(internal)` with `C0 = 2`, `w = 1 − (1 − |n|²/C0²)³` and `Φ` the exact CAM16-UCS map of the reconstructed internal stimulus (`Cam16Model::ucs_chroma_from_opponent`); `Φ(N_ref) = UCS`, so the identity Look is colorimetric on every probe and outside `|n| ≥ C0` the UCS path is published unchanged. The residual lookup coordinates are `w(|n|)·UCS + (1 − w(|n|))·n` and the residual is the 17³ cubic B-spline. The fitting residual is `w·(post-clip UCS − observed UCS) + (1 − w)·(post-clip internal − Ψ(observed))`, with `Ψ = Φ⁻¹` applied once per sample. The file fixes the knot vectors, residual domain, core terms and join radius; the exposure transition widths, the tint continuation end and `K` are evaluator constants derived from the tone range and the reference viewing conditions. Out-of-domain inputs are handled by the domain predicates and the interior barrier of [02 §4.1](02-color-pipeline.md), not by a clamp.

A body's control-response model is a separate `crm.json` (§1), scoped to `(body, epoch, native_sim)`, versioned, and referenced by the `RealizationRevision` and the family target's fit provenance.

## 4. Storage notes

- SQLite (WAL mode) for entities, replicated continuously with Litestream. Settings/metadata are JSON text validated in the application. UUIDs are stored as text. Migrations via `sqlx migrate`.
- Object storage keyed by `sha256`. Sample blobs are **private** (never published). Artifacts are public and addressed by hash.
- **Qualification associations.** Reports and dependency links are immutable; `TargetEligibility` snapshots are immutable public proof summaries; the current projection is transactionally updated and published as a hash-bound snapshot, not an immutable event stream. The derived `CurrentTargetEligibility` status is current and separate from its published snapshot id. A later report on unchanged model bytes reuses artifact hashes and creates a new report association and current projection snapshot; it never rewrites a version manifest, bundle, card or realization artifact ([05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)).
- **Bundle revisions.** `MatchBundleRevision` records and their manifest artifacts are immutable. A camera-only update publishes a new revision and updates the bundle-scope current selection; old revisions stay resolvable. Legacy target-owned bundles are not rewritten; backfill a bundle revision only where retained provenance identifies body, epoch, schema, transform, body parameters and per-sim revisions unambiguously. Unknown legacy scope remains unassessed.
- **Legacy scope.** A legacy artifact is backfilled with a subject scope only where retained provenance identifies it unambiguously. Unknown legacy scope is recorded as unassessed, never as a shared-family target or a qualified realization. Migration does not grant qualification, and historical pins stay reproducible under their existing publication policy.
- **Withdrawal:** a request signed by the contributing credential. The data is excluded from the next fit. Published versions are immutable. Blobs are hard-deleted after 30 days. Erasure for users who lost their credential is pending legal review ([10 Q3](10-open-questions.md)).
- Don't store IP addresses. Rate limiting is in memory only.

## 5. Published static data

The site is a directory of immutable files plus JSON indexes, written by `looklab-publish`. The crate has no filesystem access: `write_site` takes a `SiteSink` (one `put(path, bytes)` call per file), so the same writer runs natively and in a browser test. Look pages and the index carry `matchable` per version; a non-matchable version is listed and previewed but excluded from matching controls and match requests. Preview listing does not require an eligible target snapshot or create a selected target. Stock-settings matching, firmware-card output and validated editing-app export are unavailable for that preview. An experimental source-limited version with no supported probe stays listed, matchable and previewable; it is offered only through the explicit experimental path and carries the early-estimate notice.

```
data/index.json                          SiteIndex: bodies, interfaces, looks, scenes, input transforms
data/audit-log.json                      AuditLog (docs/05 §11)
data/bodies/<make>/<model>.json          BodyPage
data/looks/<slug>.json                   LookPage
data/looks/<slug>/<version>/manifest.json VersionManifest
data/looks/<slug>/<version>/<artifact>   artifact bytes, for example look.looklab
data/reports/<report-id>.json            PublishedReportSummary
data/eligibility/<eligibility-id>.json   TargetEligibility
data/current/<subject-key>.json          CurrentProjection
data/interfaces/<manifest-id>/manifest.json InterfaceProfileManifest
data/interfaces/<manifest-id>/definition.yaml approved or bootstrap definition bytes
data/interfaces/<manifest-id>/report.json reference round-trip report
data/input-transforms/<sha256>.yaml    installed body input-transform prior
data/bundles/<body>/<epoch>/<bundle-id>/manifest.json MatchBundleRevision
data/bundles/<body>/<epoch>/<bundle-id>/<artifact> bundle artifacts
data/scenes/index.json                   SceneIndex
data/scenes/<name>.png                   RGB16 PNG of shaped scene-linear Rec.2020 coordinates
data/scenes/<name>-thumb.png             thumbnail in the same coordinate format
```

**VersionManifest.** `{ look, version, artifacts: [{ name, sha256, bytes }], status, gate_summary, provenance }`. `status` is `provisional | rooted | published | deprecated`; `gate_summary` lists each release check and its result; `provenance` summarizes bodies and epochs, root admissions, the anchor version, the community snapshot, prep versions, the creation time and the producing software. Every artifact is addressed by the lowercase hex SHA-256 of its bytes. `verify_artifact(manifest, name, bytes)` returns `HashMismatch` when the name is unlisted or the size or digest differs.

**Client rule.** A page downloads an artifact only through its manifest and verifies it before parsing. A mismatch is an error and nothing is rendered from the bytes. The Look page shows each artifact's digest and a "matches version manifest" indicator after a successful check.

**Interface profiles.** Each published definition revision lives in a directory named by the SHA-256 of its `InterfaceProfileManifest` bytes, so a later approval of the same id writes a new revision and never overwrites an older definition or report. The manifest pins the definition and the reference round-trip report as `{ name, sha256, bytes }` entries and records whether the definition was approved through a proposal (with the proposal reference and snapshot version/hash) or shipped with the baseline. The index lists the current revision of each id; older manifest paths remain resolvable. The report is the bounded reference pipeline check of [04 §7](04-matching-and-export.md), not empirical camera qualification.

**Input-transform priors.** The index lists the current installed input transform for each body; earlier revisions remain published at their content-addressed paths. Each is addressed by the SHA-256 of its bytes at `data/input-transforms/<sha256>.yaml`; replacing a transform writes a new file and leaves the previous file and pin resolvable. These entries are installed body input priors for camera-RGB exports, not measured body calibration, realization, control-response or bundle qualification.

**Qualification reports and current selection.** Whitelisted `PublishedReportSummary` records, published `TargetEligibility` snapshots, current projections and bundle revision manifests publish under the authenticated root alongside target manifests; report membership, queryable evidence rows and dependency links remain private. A published report summary names the subject, candidate artifact hashes, capability, scope, rule revision, outcome, metrics, supported mask, limitations and evidence requests; it contains no private blob references, sample fingerprints, group identities or dependency rows. A current projection names the selected report, eligibility or bundle set per subject and use, keyed by capability and scope, with applicability and pending generation. Historical permalinks pin immutable report, eligibility and bundle revision ids and the full ranked bundle eligibility at that time; current applicability is checked separately and never rewrites the pinned ids. Later qualification of unchanged bytes creates a new association and reuses hashes; it does not rewrite the original manifest, bundle or card. Qualified matching selects only current qualified projections, while experimental discovery is a separate selector; `matchable` keeps its existing meaning and is not overloaded with qualification. A camera-local diagnostic fit is never selected as a family target ([04 §2](04-matching-and-export.md), [05 §8.1](05-trust-and-integrity.md#81-scoped-qualification), [05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation)).

**Authenticated index.** `data/index.json` is the single publication root. It lists every version's `manifest_sha256` and byte count, every published report summary, published target eligibility snapshot and current projection's `sha256` and byte count, every bundle revision manifest's `sha256` and byte count, every current interface manifest's `sha256` and byte count, every current input-transform prior's `sha256` and byte count, and every scene's `sha256` and byte count. A serverless preview embeds the digest from `site/site-root.txt` in its wasm. An online build fetches the current digest from `/data/site-root.json` at its configured API origin and fetches the catalog and publication artifacts from that same authority. The client rejects an index whose SHA-256 differs, then verifies each manifest, published report summary, published target eligibility snapshot, current projection, bundle revision and scene against the index before parsing their contents. These checks detect corruption and partial publication. The live root endpoint is trusted authority, not protection against a malicious API; a malicious application origin can also replace the client and its embedded pin. The CLI and reproducible builds provide the independent verification described in [06 §5](06-privacy-and-payload.md).

**Body page data** is derived from the body and control-schema YAML snapshots: make/model, epochs and firmware ranges, capabilities, mapping status, the control schema per epoch with control differences, native sims, plus page-only facts the schema does not hold: input-transform confidence (source, anchor count, tier), the specific preparation or fitting blocker where applicable, and plain-language realization notes. The writer joins each native sim to its pinned family look version and applicable current projection, lists bundle-scope current selections separately from target Look versions, and never uses a global maximum version that could select a camera-local diagnostic fit.

**Match bundle revisions.** A `MatchBundleRevision` is target-independent. Its manifest pins body/epoch, schema snapshot, input transform, body parameters and the ordered per-sim base realization artifact, own-family version, CRM and covariance artifacts. The target Look version is selected separately by the request or permalink. Publishing a bundle revision or updating its current selection does not create or rewrite a target Look release; a camera-only update publishes a new revision and updates the bundle-scope current projection. Old target-owned bundles and manifests remain historical, immutable and resolvable. A new bundle snapshot may change the available sims; source entries carry artifact and version references only, and a qualified selector composes the selected current report set and ranks only the settings and sims it covers, while other bundled sources never enter implicitly. Qualified and experimental selectors keep separate compatible last-good bundle snapshots, so a new experimental snapshot cannot hide an older valid qualified bundle. An incompatible sibling revision is excluded. New report qualification reuses the same bundle bytes through an association.

**Match bundles.** A target-owned version carries one bundle per `(body, epoch)`. This layout remains valid historical data; new camera-only updates use `MatchBundleRevision`. The descriptor `match-<body>-<epoch>-sims.json` (`looklab-match-bundle/1`) names, for each ranked sim, the base realization Look (`match-<body>-<epoch>-<sim>-base.looklab`), its `RealizationRevision` id pinning that sim's own native-family `look@version`, its control-response model (`...-<sim>-crm.json`) and optional predicted covariance (`...-<sim>-covariance.json`), and lists every native sim the bundle leaves out with `no_published_look` or `no_response`. The target Look version being matched is separate from each source base's family version and may be a different family or a `lut_defined` target; each source base realization is a derived evaluation artifact, not another canonical target. The schema snapshot (`...-body.yaml`) and input transform (`...-transform.yaml`) are shared. Consumers verify every artifact through the version manifest before parsing it, so a bundle is only usable as a whole. The consumer reads `manifest.json` from the version directory, verifies the descriptor against it before decoding, then verifies and parses the schema snapshot, transform, base Looks, response models and covariances from the same directory. Each descriptor artifact name must be the canonical `match-<body>-<epoch>-<sim>-...` name derived from the descriptor's body, epoch and sim, so a descriptor cannot name files outside its version directory. Each response model must name the descriptor's body, epoch and sim.

The target-owned assembly path below remains for compatibility with existing versions; new camera-only updates publish independent `MatchBundleRevision` records. The worker assembles a bundle for every active camera body epoch in the version's release provenance before writing the version manifest, from the realizations published at that moment plus the release's own in-memory candidate sim. The bundle artifacts are stored with the version and listed in its manifest; a later source-sim release, schema update or input-transform update does not change the version's bytes or manifest. A version published before bundles were bound this way has no bundle and does not gain one retroactively. A sim whose Look has no published realization on the body, or whose realization has no response model for the epoch, is listed in the descriptor with the matching reason. A hidden or removed Look contributes neither a target version nor a ranked sim. Clients verify the bundle artifacts through the manifest like any other artifact. The fit stores no control-response covariance yet, so bundles omit `covariance.json`.

**Look page data** repeats the identity (`slug`, name, kind, `wb_policy`, description, aliases, license for LUT-defined looks) and lists each version's manifest, changelog, weak spots and any publication notice (a since-revoked root admission, a revert, or an early estimate with no independently checked region). A native-family page presents the canonical target and its camera realizations separately, with each realization revision's subject, pinned family version, scope and qualification status. A `lut_defined` page shows its target eligibility basis, inherited support and uncertainty, current base applicability, `CurrentTargetEligibility` status and any invalidation notice.

**Previews.** A version may publish `preview.lut33`, a 33³ grid as little-endian `f32` over a scene-linear log2 domain: each axis is Rec.2020 linear RGB, shaped as `0.5 + 0.5 · log2(1 + x / 0.18) / log2(1 + 2⁶)` above zero and `0.5 - 0.5 · log2(1 - x / 0.18) / log2(1 + 2¹²)` below, so HDR highlights and wide-gamut colours reach the Look instead of clipping at sRGB white. Grid nodes are decoded back to XYZ, evaluated through the Look with its output clip, and stored as encoded sRGB. The browser shapes each scene pixel the same way, applies the LUT with tetrahedral interpolation, and writes sRGB to a canvas. The LUT is an artifact like any other, so it is checked before use and a look switch costs only the LUT application.

**Source-only LUT versions.** An incomplete, unmeasured or inconsistent display base publishes `source-lut.json` (`looklab-source-lut/1`), the original `source.cube` and `preview.lut33`, each bound by the immutable version manifest. The descriptor records identity, license, declared encoding and base state, output encoding, optional emulation, the source artifact and the preview composition context. The composition context records either an identified approximation of the declared base (a published matchable Look version) or the neutral fallback when no identified rendering exists. It contains no layered coefficients, and the version has no `look.looklab`, `release.json` or `card.llk`. Its preview is the original LUT composed on the recorded approximation, not an accepted target approximation. A fitted representation computed against that approximation is a separate published layered version whose `encoding` names the computation base, whose typed `declared_base` names the original declaration, and whose version marker makes it experimental. The declared base and pending rebase intent survive publication. A better approximation attempts a published approximated fitted representation when it passes convergence, the existing LUT publication accuracy gate and supported-probe checks; when it does not, it publishes a new immutable source-only version on that approximation. The exact declared base publishing as a matchable Look publishes the first exact fitted representation under the same checks. Neither step requires a moderator re-anchor merely because earlier versions were previews, and no fit compares a first candidate against itself. The ordinary URL follows the current selection, so old versions are internal rather than a public-history requirement.

**Card files.** A firmware card is a layered target artifact, never a source-only preview artifact. Download requires a publishable target, its manifested `card.llk` and, when the target publishes a support mask, a supported probe in that mask; a source-limited target with no independently checked region publishes no card ([04 §8](04-matching-and-export.md)). It is body-agnostic and checked before use under the client rule; missing destination-camera calibration, realization or CRM does not block this path.

**Canonical scenes** originate as scene-linear D65 XYZ and are stored as RGB16 PNGs of the preview shaper's Rec.2020 coordinates, quantized to 12 bits (`looklab-scene/rec2020-shaper-png16/1`). These PNGs are LUT coordinates, not display-ready images. Full scenes have at most 1600 px on the long edge; thumbnail variants have at most 400 px. The sample site publishes six CC0 RAW photographs alongside the neutral and hue gradient, hue wheel, and skin, foliage and sky test charts.

`SceneIndex` and the authenticated `SiteIndex` list each scene's `name`, typed `kind` (`photo | test_chart`), display `title`, optional `credit` (`text`, `source_url`, `license`, `source_sha256`), dimensions, `path`, `sha256` and `bytes`. The nested `thumbnail` records its own dimensions, path, digest and byte count. Clients verify full scenes and thumbnails against the authenticated index before decoding.

**Sample data.** `cargo run -p looklab-publish --features photo-scenes --example site_data` fits three synthetic Looks, builds their manifested sample camera match bundles, then computes Release audit entries from the final manifests. It publishes bodies, photographic scenes, test charts and the audit log, writing the tree above plus `site/site-root.txt` to `site/`. `publish_match_fixture` in `looklab-wasm` adds the separate `synthetic-match` test fixture without rewriting public look manifests. `sh site/scripts/build-site.sh --release --dist <dir>` embeds the root digest and produces a site that renders with no server.

`looklab publish --db <sqlite> --blobs <dir> --data data --transforms data/transforms --out site` writes the same tree from the server database: published Look versions and their manifested artifacts, the match bundles of every published body epoch, body pages from `data/bodies/`, and the release entries of the audit log under `site/data/`, with the index digest at `site/site-root.txt`. It publishes the photo scenes prepared under `data/scenes/` (one PNG per scene plus a provenance index; see [ops/deployment](ops/deployment.md)), verifying each PNG's SHA-256 first. Without that directory it publishes the test charts only.
