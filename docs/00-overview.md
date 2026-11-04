# 00 — Overview

## Problem

Camera "looks" (Fujifilm film simulations, OM System color profiles, Nikon Picture Controls, Ricoh image controls, custom firmware sims, LUTs) are mostly compared by eye or with ad-hoc color-difference checks. There's no rigorous, repeatable way to:

1. **Define** a look independently of the camera that produced it.
2. **Compare** two looks, or your firmware's sim against a reference, in a way that explains *how* they differ (tone? reds? skin hue?), not just *how much*.
3. **Reproduce** a target look on a different body, using either its stock controls or a loadable LUT.

Existing tools each cover part of this. darktable-lut-generator fits a LUT from RAW+JPEG pairs of one camera. dtsolve fits darktable module parameters to a look. colour-science supplies the color math. None of them crowdsources, separates the look from the sensor, quantifies uncertainty, or produces settings for another camera's controls. See [references](references.md).

## Goals

- **G1: Camera-independent looks.** A Look is a function from white-balanced *scene* colorimetry to display *appearance*, estimated from RAW+JPEG pairs and separated from each body's sensor/profile by explicit per-body input transforms.
- **G2: Explainable comparison.** Differences are decomposed into tone, neutral-axis tint, per-hue chroma gain, per-hue hue rotation, per-hue lightness shift and a residual. Every number comes with uncertainty.
- **G3: Actionable matching.**
  - Stock firmware: the best achievable control settings, predicted error by region, and a list of what can't be reached.
  - Loadable firmware: an exact artifact baked for that firmware's declared interface.
- **G4: Crowdsourced and robust.** Many contributors and many bodies improve targets over time. The system resists honest mistakes and deliberate poisoning.
- **G5: Private by construction.** No pixels leave the user's browser. Only shuffled, coordinate-free color samples and whitelisted metadata do, under a pseudonymous credential (no accounts, email or stored IPs). Uploads are still pseudonymous personal data, because someone holding the original photo could match them; see [06](06-privacy-and-payload.md).
- **G6: Low barrier to contributing.** Dropping in a folder of existing RAW+JPEG pairs is a valid contribution. More rigor (re-renders, phone pattern, color chart) is optional and raises weight.
- **G7: Rust end to end.** Backend, frontend (WASM) and the color/fit core. Python is allowed only for dev-time golden-vector generation.

## Non-goals (for now)

- Spatial effects: grain, clarity/local contrast, sharpening, noise reduction. These are masked or downsampled away. They could be measured separately later.
- Video looks / log profiles. The architecture doesn't preclude them.
- Public user image galleries. Post-launch; needs its own moderation design.
- Scene-adaptive pipelines (auto DR, face-aware rendering) beyond *detecting* them as outliers.

## Glossary

| Term | Meaning |
|---|---|
| **Body** | A camera model, identified by EXIF make/model aliases. Also covers software "bodies" like darktable, which have controls but no sensor. |
| **Firmware version / Epoch** | Firmware versions are grouped into **epochs**. Within an epoch, the control schema and sim behavior are assumed identical. Epochs split at breaking changes. |
| **Control schema** | Versioned definition of a body/epoch's look-related controls: names, ranges, steps, dependencies, makernote mappings. |
| **Native sim** | A look built into a body's firmware (e.g. Classic Chrome on X-T5, epoch 2). |
| **Look** | A target: function from adapted scene colorimetry to CAM16-UCS appearance. Versioned. Kinds: `native_family` (defined by native sims across bodies), `lut_defined` (defined by an uploaded LUT plus declared encoding). |
| **Realization** | A Look as produced by a specific body + native sim + settings. |
| **Input transform** | Per-body map from linear camera RGB (white-balanced) to scene XYZ. Initialized from published matrices, refined by the crowd and anchored by charts or spectral data. |
| **Recipe** | Settings on a body that *attempt* a look not native to it (or a different one). Measured, never used to define the target. |
| **Probe set** | Fixed, versioned set of synthetic scene colors on which looks are evaluated and compared. |
| **Sample** | One (camera RGB, JPEG RGB, weight) pair drawn from an aligned RAW+JPEG pair. No spatial coordinates. |
| **Root set** | Trusted reference data that bootstraps trust per look/body. Admitted only by seeder action. |
| **Contributor credential** | Pseudonymous identity: a passkey (WebAuthn), created only when contributing. No accounts. |
| **Tier** | `new` (capped as a group), `endorsed`, or `seeder`. Only a human endorsement moves a contributor out of `new`. |
| **Seeder** | Tier granted by an admin. Endorses contributors and admits root sessions within its scope. |
| **Endorsement** | Scoped, revocable, publicly logged decision by a seeder that moves a credential to the `endorsed` tier. |
| **Reputation** | Per-credential multiplier that adjusts weight within a tier and nominates contributors for endorsement. It never changes tier. |
| **Effective contributor** | Credentials merged by serial HMAC, for caps and diminishing returns only. |
| **Interface profile** | Declaration of what a custom/hacked firmware can load and where in its pipeline it applies. |
| **Bounty** | A public request for specific data, generated where the system's uncertainty most affects users. |

## Core principles

1. **Compare functions, not photos.** Every look, realization and candidate setting is reduced to a function evaluated on the same probe set. That's what makes different scenes, cameras and contributors comparable.
2. **Layered, interpretable models.** Parametric layers (tone, tint, chroma/hue/lightness by hue) plus a regularized residual LUT. The layers make the diagnostics and the "what to change" answers possible.
3. **Uncertainty is first-class.** Every prediction carries a variance from data density, fit uncertainty and cross-body disagreement. Matching weights by it, and release gates scale their tolerances by it.
4. **Bound what you can't verify.** The client is open source and can be modified, so its claims are trusted only up to the new-tier group cap. More influence requires a human endorsement. Damage that gets through is bounded by release gates and repaired by rollback, at hobby scale rather than against determined attackers.
5. **Recipes never define targets.** Recipe data may refine body input transforms and control-response models, and drives bounties. It never feeds any Look's coefficients.
6. **Nothing spatial leaves the client.** Alignment and masking happen locally. Uploaded samples are coordinate-free and shuffled.
7. **Test the central premise early.** G1 assumes one sim behaves the same across sensors once input transforms are accounted for. A two-body spike (P1.9) tests this right after the single-body milestone.
