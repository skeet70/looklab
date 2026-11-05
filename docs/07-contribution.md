# 07 — Contribution

**Principle:** a very low bar to start, and rigor that's optional, incremental and can be added later (it applies retroactively). Leaving at any point keeps what's already contributed. Weight comes from the contribution's kind and measured coverage ([05 §5](05-trust-and-integrity.md)). Kinds are claimed by the client and bounded by the new-tier cap, and only a seeder's endorsement moves a contributor out of that capped tier.

No account is needed. At the first upload, or earlier from the sign-in dialog ([08 §4.1](08-ui.md)), the browser creates a passkey, and the contributor gets a status link ([06 §4](06-privacy-and-payload.md)). Platforms without a built-in passkey store (common on Linux desktops) can use a phone via the cross-device QR flow, a security key, or a password manager that stores passkeys; the page says so before the user starts.

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

- **Pair** RAW↔JPEG by base filename, then capture time (subsecond where available) and the same body. Show unpaired files with a reason.
- **Attestation signals** (consistency inputs, not proof):
  - The makernote sim tag matches the declared sim (the makernote is the default; the user confirms).
  - The `Software` tag equals the body's firmware string; anything else means "edited" and the image is excluded.
  - Makernotes are present in the JPEG.
  - C2PA content credentials, where present and valid, are post-launch evidence. Treat them as a server-verifiable signal only once verification runs server-side on the embedded manifest.
- **Usable settings** ([03 §1](03-data-model.md)): any values of `look_affecting` controls are usable. Default-setting images are target-defining, and non-default images inform the Look through the joint fit. Spatial controls must be at a `tolerated` value.
- **In-camera re-renders:** the client recognizes JPEGs rendered from the same RAW (same capture time/image number with different settings, or the conversion's own markers; verify per make). It assigns a `rerender_group` and **samples one shared pixel set** for the group, which gives the control-response fit exactly matched inputs ([03 §2.1](03-data-model.md)).

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
- Even light, no glare, chart at 30–60% of the frame, bracket 0/−1/−2 EV.
- Assisted detection (click 4 corners, client refines); automatic later. Patch means from the central 50%, with glare rejected by std.
- Reference values with source and edition in `data/charts/`.

## 4. Recipe and card contributions

Intent `recipe`: settings on body B attempting target Look L. The flow is the same, with the target prefilled from a settings card. Used for body parameters, the control model, recipe scores and bounty analysis. **Never** for L's target ([05 §12](05-trust-and-integrity.md)).

Intent `card`: a JPEG rendered by hacked firmware from a card file ([04 §8](04-matching-and-export.md)). The client sets it from the render tag ([04 §8.4](04-matching-and-export.md)); when the tag is missing, the user declares it by picking the firmware. Used only for per-firmware verification scores, grouped by firmware id and version. **Never** for any Look target, body parameter or control model: the makernotes name whatever stock sim the firmware borrowed, not the Look the image shows.

## 5. Contributor feedback

Via the status link: accepted (weight tier) / down-weighted (reason) / quarantined (reason + how to fix), e.g. "This looks like auto DR was on; set DR manually and try again." Keep the wording plain.
