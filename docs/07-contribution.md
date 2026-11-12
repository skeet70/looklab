# 07 — Contribution

**Principle:** a very low bar to start, and rigor that's optional, incremental and can be added later (it applies retroactively). Leaving at any point keeps what's already contributed. Weight comes from the contribution's kind and measured coverage ([05 §5](05-trust-and-integrity.md)). Kinds are claimed by the client and bounded by the new-tier cap, and only a seeder's endorsement moves a contributor out of that capped tier.

No account is needed. At the first upload, or earlier from the sign-in dialog ([08 §4.1](08-ui.md)), the browser creates a passkey, and the contributor gets a status link ([06 §4](06-privacy-and-payload.md)). Platforms without a built-in passkey store (common on Linux desktops) can use a phone via the cross-device QR flow, a security key, or a password manager that stores passkeys; the page says so before the user starts.

Intake is broad and has no camera allowlist: any supported body can contribute, and its camera page and aggregate model status are discoverable before it has a qualified model; private samples stay private. Acceptance, promotion, invalidation and evidence requests run automatically through the worker from scoped checks; a moderator does not promote a camera model, and upload counts, mapping confirmation or root admission are not empirical checks ([04 §1](04-matching-and-export.md#1-matching-levels), [05 §8](05-trust-and-integrity.md#8-release-gates-and-rollback)). Contribution depends on the existing decoder, published control schema and intake resources; a missing one is reported as an exact blocker. Browser preparation uses the RAW's own camera matrices, so a missing server input-transform prior does not block it; server-side colour fitting still requires the published prior and never substitutes another matrix for it.

## 1. The ladder

| Rung | Ask | Effort | Base weight | What it mainly improves |
|---|---|---|---|---|
| 0 | **Point at your library:** drop a folder; the app pairs files, reads sim/settings from makernotes, and preselects | ~1 min | 0.25 | Look coverage, body parameters, control model |
| 1 | **A few requested shots** for coverage gaps ("saturated blue", "a face in shade") | 5–10 min | 0.25 (not distinguishable from library), but high coverage gain | Targeted coverage |
| 2 | **Re-render in camera:** 3 chosen RAWs through your other sims and/or a control sweep | ~2 min | 1.0 | All native Looks on the body; the control-response model |
| 3 | **Phone pattern session** (QR → full-screen pattern with a server-issued seed; 3 bracketed shots) | ~3 min | 1.0 | Saturated coverage |
| 4 | **Tone gradient:** one defocused gradient shot | ~2 min | 1.0 | Tone, neutral tint |
| 5 | **Color chart session** | ~5 min | 2.0 | Input transform; a seeder can review it for endorsement and root admission (only endorsed/seeder charts anchor the gauge) |

The UI presents the rungs as "ways to help" with time and impact (low, medium, high), without rung numbers or weights ([08 §4.2](08-ui.md)).

**Rung 0 preselection.** Diminishing returns make the 500th image worth a fraction of the first, so don't fully process everything. The client estimates each pair's coverage gain cheaply from the RAW's embedded preview or the JPEG (locally, never uploaded). It fully processes the top images in order of gain, up to a default of 100 per body/sim group, and offers "process more" with the predicted marginal value.

After each step, show the impact (server-computed, via the status link) and offer the next rung without pushing it. Retroactive upgrades: a chart session improves the body's input transform, which raises the value of earlier uploads; say so.

## 2. Pairing, attestation and usability

- **Pair** RAW↔JPEG by base filename, then capture time (subsecond where available) and the same body. Show unpaired files with a reason. Keep re-renders with different sims, settings or rendering firmware. Suppress a duplicate only when its sim/settings and known rendering firmware match; unknown firmware is retained for review.
- **Attestation signals** (consistency inputs, not proof):
  - The makernote sim tag matches the declared sim (the makernote is the default; the user confirms).
  - The JPEG's native-renderer `Software` metadata identifies its rendering firmware when available. An older firmware version in the source RAW does not make a later camera re-render edited. Software identifying an external editor remains an exclusion signal.
  - Makernotes are present in the JPEG.
  - C2PA content credentials, where present and valid, are post-launch evidence. Treat them as a server-verifiable signal only once verification runs server-side on the embedded manifest.
- **Usable settings** ([03 §1](03-data-model.md)): applicable `look_affecting` controls need reproduced values or explicit declarations. Colour default-setting images are target-defining; non-default images inform the joint fit, and spatial controls must be at a `tolerated` value. Monochrome measurements instead enter the deferred pool; grain-on renders keep their settings without establishing eligibility for a future mean-response fit. A reproduced or declared value makes a setting usable; it does not qualify that setting's response, and default-only evidence qualifies no nondefault control.
- **In-camera re-renders:** the client recognizes JPEGs rendered from the same RAW (same capture time/image number with different settings, or the conversion's own markers; verify per make). It assigns a `rerender_group` and **samples one shared pixel set** for the group, which gives the control-response fit exactly matched inputs ([03 §2.1](03-data-model.md)).
- **Optional menu confirmation:** for a camera with documentation-derived, unconfirmed capabilities, offer a collapsed disclosure asking which options the owner checked and allowing corrections in menu terms. The Send action is available without opening it. Bind the reported firmware and exact camera/control definitions. This claim is separate from settings reproduced from the files and does not affect the photos' scientific weight. One approval covers the photos and these selected declarations. Documentation-only cameras offer the same confirmation without requesting RAW/JPEG files. The send envelope is version 4; a version 3 envelope is rejected before nonce consumption with `ENVELOPE_VERSION_UNSUPPORTED` and the action `reload_and_prepare`. An old tab running the previous client only shows `upload: HTTP 426` and must be reloaded and prepared again; it has no new guidance.
- **Proposal evidence:** camera and mapping proposals may attach original camera files as CC0 fixtures, one explicit opt-in per file, with the exact size and sha256 confirmed before upload. A fixture-backed camera proposal needs at least one matching donation; a mapping without one stays provisional. Schema proposals start from the published control schema, or from a complete document for a registered body without one, and submit the complete validated document. A custom-look proposal carries the `.cube` file in the same framed, signed request as its metadata; the server checks the bytes against the signed hash and size and approval enqueues the existing LUT import path.

The browser retains File/Blob handles and scans metadata one file at a time. It reads bytes for active worker groups, decodes each shared RAW once, decodes the group's JPEGs to align and mask their shared field, and then samples them. Each JPEG keeps its own rendering metadata.

The Process step shows active progress only while preparation runs. Revisiting it after successful preparation shows completion and allows Next to return to Review without processing again. An interrupted or failed run cannot advance to Review, even if it produced some partial groups.

**Monochrome model development.** A separate selection, unchecked by default, authorizes contributing complete B&W measurements for developing and validating monochrome response, toning and uncertainty, with a 180-day deferred-retention limit ([06 §4](06-privacy-and-payload.md)). It is covered by the same send approval as colour contributions. One selection covers every B&W batch in the drop and sets each batch's own authorization, which stays the manifest's source of truth; colour batches are unaffected, and a restored draft with a partial selection stays partial until the contributor clicks the control. The contributor may select the RAW with just its B&W JPEGs, or a mixed colour/B&W re-render group. Without authorization, B&W renders stay local. The client retains RGB measurements, original native sim, normalized family and filter, WC/MG toning, grain and each setting's source; no source photos or RAW hashes are sent. There is no promise of a published Look.

## 3. Capture protocols (shown only when a rung needs them)

Shared tips (rung 1 and above): turn off in-camera lens corrections where possible; shoot RAW+JPEG; lowest native ISO; avoid motion.

### 3.1 Phone pattern (rung 3)
- The contribution flow requests a **server-issued, signed session seed** and shows a QR code to `/pattern/<seed>`. The pattern page is static and renders deterministically from the seed.
- Layout:
  - 4 corner fiducials;
  - a session-ID strip along the short side, which also fixes the display's orientation;
  - a 96-patch grid spanning the display gamut, in seed-determined order: 8×12 in portrait and 12×8 in landscape, so patches stay near square;
  - a content box that follows the viewport orientation and aspect, clamped to between 9:21 and 21:9;
  - large patches, so defocus is fine.
- Instructions: max brightness; disable night shift / true tone / adaptive color; fill ~60% of the frame; defocus slightly; 3 shots at 0, −1 and −2 EV.
- Use: coverage only. Display primaries are unknown, so it isn't an anchor. The server-issued seed ties the shots to the session.
- Original design; don't copy dtsolve's chart generator (unlicensed).

### 3.2 Tone gradient (rung 4)
- A defocused, smooth gradient from clipped white to black, under a single light source with WB set to it. Roughly uniform histogram. It must clip in the RAW. (Protocol adapted from dtsolve's documentation.)

### 3.3 Color chart (rung 5)
- v1: ColorChecker Classic 24, SG 140, Passport.
- Even light, no glare, chart at 30–60% of the frame.
- Use one built-in look, such as Provia / Standard on Fujifilm, and WB set to the light. Take three RAW+JPEG Fine shots total at 0/−1/−2 EV; keep all three original pairs. No separate bracket per look is needed.
- Optional extra coverage: re-render only the 0 EV RAW in camera using other built-in looks, keeping the other settings unchanged, and include those JPEGs with the originals. The −1 and −2 EV RAWs do not need re-renders. The donated-fixture protocol requests every film simulation ([fixture shot list §4](../fixtures/SHOT-LIST.md#4-color-chart-session)).
- Assisted detection (click 4 corners, client refines); automatic later. Patch means from the central 50%, with glare rejected by std.
- Reference values with source and edition in `data/charts/`.

## 4. Recipe and card contributions

Intent `recipe`: settings on body B attempting target Look L. The flow is the same, with the target prefilled from a settings card. Used for body parameters, the control model, recipe scores and bounty analysis. **Never** for L's target ([05 §12](05-trust-and-integrity.md)).

Intent `card`: a JPEG rendered by hacked firmware from a card file ([04 §8](04-matching-and-export.md)). The client sets it from the render tag ([04 §8.4](04-matching-and-export.md)); when the tag is missing, the user declares it by picking the firmware. Used only for per-firmware verification scores, grouped by firmware id and version. **Never** for any Look target, body parameter or control model: the makernotes name whatever stock sim the firmware borrowed, not the Look the image shows.

Rendering classification is independent of intent. A monochrome native, recipe or card render remains that intent while awaiting analysis; none updates colour fitting, body calibration, release scores or standing. Card measurements do not become native evidence. Experimental publication changes neither rule: a recipe or card render never defines or retrains a target, and a native-family cohort admission never retrains a `lut_defined` composition ([04 §2](04-matching-and-export.md#2-realization-model-on-a-body-stock-firmware)).

**Declared settings.** A control the snapshot maps no makernote tag for, or whose tag the file cannot decode, is *declared*: the contributor enters the value, and the batch records it with source `declared`; values decoded through the schema are `reproduced`. The client does not upload a group while a look-affecting control has neither source, because the fit could not use it. It names the control ("this body's <control> isn't mapped yet"), offers the declaration, and may keep the pair locally to offer again once a mapping ships. A declared value is a claim like the contribution kind: the existing tier caps, outlier scoring and reputation apply unchanged, and v1 gives it no extra or reduced weight. It adds no field to the upload envelope ([03 §2.3](03-data-model.md#23-upload-envelope-canonical)). Card renders are exempt from covering every look-affecting control because they are never fitted. "Unmapped" is decided against the snapshot version named in the batch manifest, so a client on the previous snapshot is not judged by a mapping that just landed ([03 §1](03-data-model.md), [06 §5](06-privacy-and-payload.md)).

## 5. Contributor feedback

Via the status link: accepted (weight tier) / down-weighted (reason) / quarantined (reason + how to fix), e.g. "This looks like auto DR was on; set DR manually and try again." Deferred B&W records say **"received, awaiting monochrome analysis"**, without consistency or reputation credit. Keep the wording plain.

Feedback also reports scoped model status. A contribution can be accepted while its camera's model remains experimental for settings the evidence does not cover; that is not a rejection of the contribution. When a scoped claim that used the contribution is invalidated by withdrawal, exclusion, an identity merge, a revoked root admission or a changed schema or profile interpretation, the status names the claim that changed and the evidence that would restore it. Experimental diagnostics never change the contributor's scientific reputation or the consistency state used by qualified fitting ([05 §5](05-trust-and-integrity.md#5-weighting)).

Wanted shots are generated automatically from missing evidence: an unassessed setting or combination, independent captures, a colour or exposure region, or a calibration source. A request is not a moderator promotion ticket, and no camera-wide approval exists. Qualification changes neither the upload payload nor retention: no new field, RAW, coordinate or purpose ([06 §1](06-privacy-and-payload.md#1-what-is-uploaded), [06 §4](06-privacy-and-payload.md#4-status-withdrawal-and-erasure)).
