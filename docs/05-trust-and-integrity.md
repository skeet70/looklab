# 05 — Trust and integrity

**Goal (hobby scale):** reasonable contribution weighting, transparent anti-spam and a low moderation load, with no user accounts.

**Non-goal:** resisting a determined, patient attacker. Such an attacker can nudge regions that are already labeled provisional by a bounded amount. Caps bound the damage, release gates stop it accumulating, and **cheap rollback** repairs it (§8). We don't try to prevent it outright.

**Design in one paragraph:**
- **Identity:** passkeys, only for contributing.
- **Weight:** image weight = kind × evidence × tier × reputation, then caps.
- **Trust:** only a human endorsement moves a contributor out of the capped new tier. Reputation nominates contributors for endorsement but never grants trust by itself.
- **Serial merging:** can only *reduce* weight, through caps.
- **Releases:** gated against the previous version and cumulatively against a frozen anchor, with a one-click revert.
- **Qualification:** measured behaviour is scoped to an immutable target or camera-realization revision and independent evidence. It is separate from publication eligibility and human-granted trust (§8).

## 1. Threat model

- The client is open source and can be modified, so **contribution kinds (chart, pattern, gradient, re-render) are client-claimed**. At this scale that's acceptable: a false claim from a new-tier credential only shifts weight *within* the capped new-tier pool (§6). Server re-verification of kinds can be added later if needed.
- The published models (Looks, input transforms, control-response models) let anyone synthesize data that agrees with them. That can farm reputation (§4), but reputation can't lift a credential out of the new tier, so the effect is bounded by the cap.
- Honest mistakes (wrong sim, auto DR, edited JPEGs) are handled by the per-image consistency checks ([02 §4.4](02-color-pipeline.md)). They apply to every tier, seeders included.

## 2. Identity

- **Passkeys (WebAuthn) only, and only for contributing.** Browsing, matching, previews and the verification loop are anonymous and need no credential. Server side: `webauthn-rs`.
- **Platforms without a built-in passkey store** (common on Linux desktops) can use a phone via the cross-device QR flow, a security key, or a password manager that stores passkeys. The contribute page says so up front.
- **Signing:** one passkey assertion per **send**, not per batch. The client builds a canonical send manifest listing every batch's manifest hash and signs `H(server_nonce ‖ send_manifest_hash)` once ([03 §2.3](03-data-model.md)). Every batch request carries that send manifest and the same assertion; the server binds the first accepted batch's credential and assertion digest to the send instance and rejects a batch whose send manifest, credential or assertion differs, a replayed send, and a batch that already arrived. The nonce is per send; the same prepared batches under a fresh nonce are a new send. Withdrawal/deletion requests are signed the same way, over their own domain digest.
- **Lost passkey:** the contributor starts again as new, and a seeder can re-endorse them. Synced passkeys make this rare.
- **Serial merging:**
  - The client sends the body serial (contributors may opt out). The server immediately computes `HMAC-SHA256(server_secret, make|model|serial)`. The raw serial lives only in a type that can be neither serialized nor printed with `Debug`, and it's never stored or logged.
  - Credentials whose uploads carry the same serial HMAC form one **effective contributor**, **only for caps and diminishing returns** (§6).
  - **No transfer:** merging never transfers reputation or tier between credentials. A serial copied from someone's public EXIF can't hijack their standing.
  - **Cap order:** when merged credentials have different standing, the cap is applied to the lower-standing credential's images first.
  - A serial that appears under many credentials raises a **moderation flag**.

## 3. Tiers, roles and endorsements

| Tier | How you get it | Tier factor *(tentative)* | Subject to new-tier group cap |
|---|---|---|---|
| `new` | First contribution | 1.0 | yes |
| `endorsed` | A seeder endorses you (scoped by rules) | 2.0 within scope | no |
| `seeder` | An admin grants it | 2.0 within scope | no |

- **Roles** (separate from tiers): `moderator` and `admin`. Admins grant seeder and moderator through `looklab-cli admin …` or an admin page. Seeders endorse contributors in the app.
- An endorsement records scope, reason and evidence (e.g. a reviewed chart session), and is revocable. **Outside its scope, an endorsed credential counts as `new`** (tier factor, group cap and all). The same applies to a seeder outside its scope.
- A scope is a list of rules `{body, look}`, where either side may be `*`. An image `(body, look)` is in scope when any rule matches: `(B, *)` covers every Look on that body, `(*, L)` every body for that Look, and `(B, L)` only that pair. Each image gets one endorsement factor from its `(body, look)` pair, used everywhere that image appears in the fit. The default scope for chart-session evidence is `(B, *)`.
- **Every grant, endorsement and revocation** is a database row plus an audit-log entry (§11). The lists are public, identifying credentials by internal id or chosen display handle, never by public key.
- Seeders, moderators and admins should use hardware-backed passkeys or security keys.

## 4. Reputation

- Range [0.2, 1.5]. Starting values: new 0.5, seeder 1.2. An endorsement keeps the current value.
- After each release in an empirically qualified scope in which the contributor had eligible data:
  ```
  r ← clip(r + η·(a − 0.5) − penalties, 0.2, 1.5),   η = 0.1
  ```
  - `a` = the fraction of the contributor's images in that release accepted without down-weighting.
  - Penalties: 0.2 per quarantine judged deliberate by a moderator, 0.05 per automatic quarantine.
  - Bonus: +0.05 per chart session that passes consistency.
  - Decay toward 0.5 with a 1-year half-life.
- **Reputation raises weight only within a tier.** It never moves a credential out of the new tier and never bypasses the group cap. Farming it with synthetic data is therefore bounded.
- **Reputation nominates; people decide.** High-reputation new-tier contributors appear in seeders' endorsement queue.
- Deferred monochrome measurements earn no reputation, consistency bonus or endorsement evidence. Their status is "received, awaiting monochrome analysis", not accepted as consistent.
- Display: a coarse tier/badge plus accepted-contribution counts.
- Model-based acceptance fractions, penalties and bonuses from experimental diagnostics do not update scientific reputation or consistency state used by qualified fits (§8.2).

## 5. Weighting

```
w_img = base(kind) × evidence × tier × reputation        → then caps (§6)
```

| Kind (client-claimed) | base |
|---|---|
| `library` (incl. requested shots) | 0.25 |
| `rerender` | 1.0 |
| `pattern` | 1.0 |
| `gradient` | 1.0 |
| `chart` | 2.0 |

- `evidence = coverage_gain^0.5 × (1 − clip_fraction)`, clamped to [0.1, 1]. Computed **at fit time** from the samples, against the density of the rest of the pool **with this image left out**, so it doesn't depend on upload order.
- **σ_v is in the likelihood only** ([02 §4.1](02-color-pipeline.md)). It's not a weight factor.
- Per-image consistency checks apply regardless of tier. Diagnostics in an unqualified scope remain experimental rather than automatically changing qualified consistency/reputation state (§8.2).
- **Duplicates (honest):** images from the same effective contributor with the same capture-metadata tuple are merged into one weight. Shuffling makes uploads byte-different, so there's no byte-level comparison.
- **Chart anchoring of input transforms:** only chart sessions from `endorsed` or `seeder` credentials enter as gauge anchors with tight priors ([02 §2](02-color-pipeline.md)). New-tier chart sessions count as ordinary data, subject to the body-level cap (§6).
- **Deferred monochrome:** scientific fit weight is zero. These records are excluded from colour Look parameters, input transforms, shared body exposure/WB parameters, gauge estimates, colour uncertainty, supported-image counts, cap denominators and release scores. The worker enforces this on every analysis entry point, including stale jobs; intake rejects a signed colour classification that conflicts with the schema or rendering metadata.
- A shared RAW establishes correspondence, not an exposure reference. Monochrome re-renders remain correlated captures, even with different filters. Future monochrome analysis may consume a frozen, versioned colour calibration; feedback into shared calibration requires separate evidence.

## 6. Caps

- **New-tier group cap:** all new-tier data together gets at most **γ = 30%** *(tentative)* of the weight:
  - per Look, **per region** (12 hue × 6 exposure cells over supported probes);
  - per body, for the body parameters (input transform, ξ, ω, control-response model), because a body update moves every Look on that body.

  In regions or bodies with no endorsed or root data, new-tier data defines the value, and the region/body stays **provisional**.
- **Per effective contributor:** with `n` effective contributors in the pool, at most `max(0.05, 2/n)` of a Look version's weight and `max(0.20, 2/n)` of a body input transform's weight, applied by water-filling against the final total alongside the group cap. Diminishing returns `1/sqrt(1 + n/20)` also apply on the n-th image of one contributor. The floors and the factor 2 are tentative ([10 Q12](10-open-questions.md)).
- **Minimum pool (provisional label only):** a Look or body is labeled provisional when its pool has fewer than 20 (Look) or 5 (body) effective contributors and it is not rooted (§7). The threshold does not switch the per-contributor caps off.
- These pool-size and root rules describe trust provenance, not empirical qualification. More contributors, a rooted label or an accepted schema do not qualify an unmeasured control, firmware epoch or colour region (§8.1).
- Colour and deferred monochrome use the same credential, effective-contributor merging, signatures, per-batch proof of work and intake/storage limits. Changing render classification does not reset a quota. Once monochrome measurements influence a model, the existing tier and cap policy applies to that model's evidence; a shared body fit must cap the combined evidence entering it.
- **Fake serials** let an attacker take a larger share of the new-tier pool from honest new contributors, but never more than the group cap.

## 7. Root set

- The root set is **anchored sessions from endorsed or seeder credentials, admitted by a seeder** in the app (logged):
  - for a **Look**: chart, gradient and pattern sessions;
  - for a **body's input transform**: chart sessions or published spectral sensitivity data **only**. Gradient and pattern sessions don't constrain it.
- A Look becomes `rooted` once it has admitted root sessions covering its supported regions. A rooted Look is not provisional even when its pool has fewer than 20 effective contributors.
- **Sunset:** once a Look has ≥ 20 endorsed contributors across ≥ 3 bodies, the root is re-derived from admitted anchored sessions of endorsed contributors. High reputation alone never qualifies.
- **Bootstrapping (operational):**
  - One chart + gradient + pattern session per body, re-rendered in camera through every native sim, covers all of that body's native Looks.
  - Rent bodies for key Looks.
  - Use published spectral sensitivity data to anchor input transforms for bodies nobody owns.

Any supported body's eligible native evidence may seed a previously unseen family's single provisional target under the existing resource, trust and persisted gauge rules. Selection is fenced against competing initial heads. The milestone reference body is not an allowlist, and seeding grants no root authority or empirical proof of sensor independence. The explicit seed cohort may continue refitting as eligible evidence arrives; adding another experimental body is a separate admission, not a silent expansion of that cohort.

## 8. Release gates and rollback

**Anchor:** the first rooted version of a Look, or the last version a moderator explicitly re-anchored (the re-anchor action requires a reason, e.g. a new root session). Re-anchoring is needed so that legitimate long-term improvement isn't blocked forever.

**Release scope.** A canonical native-family target and a `(body, epoch, native_sim)` realization have separate immutable revisions and selected histories ([03 §1](03-data-model.md#1-entities)). A realization pins its target and calibration/CRM dependencies. Previous/anchor lookup, support history, attempts, resumable state and holds use that subject, not just a shared Look slug. Cohort updates preserve the family's anchor and gauge; scope splits retain applicable parent comparison lineage.

When no historical comparison exists, the corresponding gate is initialization/not applicable. Comparing a candidate with itself supplies no empirical evidence. An independent camera-local diagnostic fit cannot become the family target or its predecessor through maximum-version selection.

**Gated set.** The gated probes are the **sticky union**: every probe supported in any version published since the current anchor, plus every probe in either compared version's supported set. A probe that loses support for any cause stays gated until a re-anchor, which restarts the union from the new anchor's supported probes. So making a region uncertain, thin or biased cannot take it out of the gate.

**Support.** Each probe carries a pre-clip prediction (the `J'a'b'` the Look defines) and a post-clip prediction in the version's reference output encoding (pre-clip `J'a'b'` → output RGB → soft clip → `J'a'b'`), each with its own σ, and prior-attributable bias in ΔE. Support requires all of the following ([02 §5](02-color-pipeline.md#5-uncertainty)):

- post-clip `σ ≤ 1.5` entering and `σ > 1.65` leaving;
- its bias is at most `0.33 × 1.5` ΔE entering and above `1.1 ×` that leaving;
- its region reaches the absolute density floor (tempered effective weight per bin, not a share of the mean bin) and it has at least 3 distinct-image clusters. Neither has hysteresis.
- independent held-out model-error evidence for that output encoding and colour/exposure cell.

Enter and exit thresholds differ so that honest boundary jitter does not flip a probe between versions, and the previous published version's per-cause state is an input to marking. The first version uses the enter rule only. The absolute floor means data uploaded elsewhere cannot push a rare region below it.

A candidate target or realization is eligible for publication only if its required numerical and uncertainty computations pass and the applicable release comparisons below pass:

1. **Root non-regression:** median ΔE on root probes rises by ≤ 0.3 (p95 ≤ 0.5) vs. the previous version, **and** by ≤ 0.5 (p95 ≤ 0.8) vs. the anchor. Root error is measured on post-clip predictions against the admitted observations.

2. **Bounded drift, scaled by uncertainty.** Deltas are computed after converting the reference version into the candidate's gauge ([02 §2.1](02-color-pipeline.md)). A `release.json` written in the relative convention names `median(x_i)`, and only that frame converts exactly. A record from the earlier absolute-header convention cannot be recovered because the paired per-image nuisances needed to rebuild its relative median were not stored. A gate comparison that needs such a previous or anchor holds: nothing is published, the existing artifacts and anchor stay unchanged, and a moderation item records that a moderator-approved audited re-anchor and refit is required. Drift is the post-clip ΔE at each gated probe. For each such probe p, the shift |Δ_p| is compared with a bound that grows with the uncertainty of the two versions, and an absolute ceiling C:
   ```
   vs. anchor:    b_p = clamp( 2·sqrt(σ²_anchor(p) + σ²_cand(p)), 1.0, C = 3.0 )
   vs. previous:  b_p = clamp( 2·sqrt(σ²_prev(p)   + σ²_cand(p)), 0.75, C = 2.0 )
   ```
   Pass only if, for each comparison:
   - **no** probe present in both versions exceeds the post-clip ceiling C (absolute; no allowance, and it applies outside the gated union too);
   - ≤ 2% of gated probes exceed their b_p (these may only land between b_p and C);
   - the p95 of |Δ_p|/b_p ≤ 1.0;
   - **every** probe the reference version identifies keeps its pre-clip lightness shift |ΔJ′| and hue-angle shift `ΔH_ab = 2·sqrt(C₁·C₂)·sin(Δh/2)` (CIEDE2000's hue difference, UCS units scaled by chroma) within C; identification is the reference encoding's soft clip keeping a per-channel derivative of at least 0.25 at the reference prediction, so a candidate cannot exempt a probe by pushing its own prediction into the clip shoulder. An unidentified pre-clip value takes its variance through the prior floor and is gated on its post-clip ΔE only; `J′` and hue survive the reference encoding into exports and card renders ([04 §8.1](04-matching-and-export.md));
   - **no** probe that was supported leaves support because its σ rose: the candidate fails the σ test at the exit threshold (`σ_cand > 1.65`) and `σ_cand > (1 + 0.1)·σ_ref`, where σ_ref is the post-clip σ in the version that supported it. Conflicting uploads raise σ well past that margin; honest boundary jitter does not. Since b_p sits at its ceiling for such a probe, only C bounds it. Such probes count as violations and go to moderation.

   A support loss for any other cause (density floor, cluster count, bias, or σ within the margin) is a **coverage change**: it is reported in the gate report and stays in the gated union. Neither kind of loss removes a probe from drift gating.

   The floor keeps well-measured regions tight. The ceiling keeps the gate meaningful in uncertain, provisional regions, which is where a nudging attacker would aim.

3. **Changelog:** data added (counts by tier), largest contributors by weight (internal ids), gate metrics, bodies/epochs affected, input-transform versions.

A failed gate records a rejected candidate and opens a moderation item; a provisional label does not permit publication. A native fit that does not converge never reaches the gate: nothing is published, the worker resumes that subject from its stored fit state in the next batch, and 3 consecutive non-converged batches *(tentative)* open one "Fit not converging" moderation item, resolved by that subject's next published version. A failed replacement does not invalidate a predecessor whose dependencies remain valid.

**Rebase.** Each published `lut_defined` version records its declared base in `lut_base_pins`; a fitted approximation also records the identified computation base as its composition base. Pins reject self-edges and cycle-closing Look dependencies before they are recorded, so no pin creates an automatic rebase loop. A `family_rendering` base that declares settings, for example `dynamic_range: dr400`, stays pending; it is not pinned to a bare native `look@version` unless that native release demonstrably carries those settings. A base publish, a family-base resolution change, or a revoked base root writes `lut_rebase_pending` and enqueues a `lut_rebase` job, which the worker consumes without waiting for a nightly image batch. The candidate re-imports the stored LUT against the resolved `look@version` base document, not the neutral preview, and inherits the base's uncertainty and support through the importer. It is gated against the Look's first published version or last re-anchor with the existing `gate_release` inputs; root observations are empty for LUT Looks. A first fitted representation from a source-only history uses the ordinary convergence, accuracy and support checks instead of a candidate-against-itself drift gate. Only a passing candidate gets a manifest and artifacts. A failed gate writes a `gate_failures` row, the existing moderation item, and leaves the previous version's bytes and matchability untouched. Pins are never re-anchored automatically. A revoked root's audit payload gains `lut_dependents: [{look, version}]`, including transitive LUT dependents, and `RootRevocation` exposes the same list; `lut_dependents` is empty when no LUT was pinned to the affected base versions.

**Rollback:** one moderator action, "revert to version N, reject contribution(s) C, refit". A rejection must name a contribution from the reverted Look's evidence: its recorded dependency closure, an explicit target Look, or the same body. The revert publishes N's content as a new version immediately and invalidates only the rejected contributions' recorded dependents. It does not exclude the contributor's other contributions, change standing, or rewrite published versions. If N depends directly or indirectly on a rejected contribution, or its recorded dependency history cannot establish that it is clean, the new version is published as a deprecated historical copy that needs rebuilding, never as the current target; the rejection, rollback, audit and successor refit still commit. A clean N becomes the current target. A separate future-intake block denies new contributions from a credential without revoking it: withdrawal, status reads, sign-in and existing data or qualification stay available. Existing `look_credential_exclusions` rows remain historical audit data and are no longer consulted. Published versions stay immutable and pinnable. A revert is not a re-anchor: the gated union continues across it.

**Publication.** A moderation hide, remove or relist, qualification association or invalidation notice records a publication request that survives the running publisher, and the publisher's successor is enqueued in the transaction that completes the current run. Report/status-only changes require publication even without a new Look version.

**Release artifacts.** A target version publishes its Look document (`look.looklab`), preview LUT, card file and release record. Camera-realization revisions bind exact native-family target, input-transform, native/recipe realization and control-response artifacts ([03 §1](03-data-model.md#1-entities)). Each model/bundle artifact is listed by hash in its owning manifest.

An immutable `MatchBundleRevision` for `(body, epoch)` publishes independently of target versions, with its own manifest, compatible shared calibration and ordered per-sim realization artifacts ([03 §5](03-data-model.md#5-published-static-data)). Each source sim pins its own native-family version; the requested target is separate and may belong to another family or be LUT-defined. Camera-only updates create new realization/bundle revisions, not artificial target releases. Qualified selection cannot acquire experimental sibling sims implicitly. Missing or incompatible sims are excluded with reasons. Historical target-owned bundles remain immutable and resolvable; new siblings, reports or resources never rewrite a prior manifest, bundle or card. Relisting restores discovery under current eligibility, not empirical qualification.

### 8.1 Scoped qualification

Preparation availability, numerical validity, empirical qualification, publication eligibility and rooted trust are separate axes. A numerically valid, gate-passing experimental realization may be published against a publishable pinned target. A failed or non-converged candidate may expose permitted diagnostics, not accepted render/match artifacts. Missing resources and missing empirical evidence have different reasons and evidence requests.

Qualification reports bind the exact candidate, private evidence roles, schema/epoch/profile interpretation, source implementation/options, probe set, rule version and assessed settings/conditions ([03 §1](03-data-model.md#1-entities)). The three common protocols are conditional native prediction, controls and matching, and cross-body pooling ([02 §5.1](02-color-pipeline.md#51-qualification-claims-and-protocols)). Default-setting evidence does not qualify nondefault controls; numeric endpoints or separate control checks do not prove untested intervals or combinations. No empty or unavailable assessment passes, and no report proves absolute calibration from a source label or self-consistency.

P4.3's known-reference uncertainty coverage is an engine-version prerequisite for qualified multi-body activation, not a latent-truth requirement for each new camera. Camera/domain admission uses the scoped P1.9 agreement and independent discrepancy/support evidence under pinned calibration assumptions. Admission schedules a new qualified-cohort family fit; only a passing candidate becomes selected. It does not copy a camera-local diagnostic target into the family. A seed cohort's source-limited updates do not claim cross-body qualification.

The worker promotes passing scoped assessments automatically and keeps missing-evidence requests current. Qualification does not grant a tier, endorsement, root admission or re-anchor. Missing evidence is not a camera-promotion moderation queue. Keep fixed versioned protocols, immutable reports, dependency links and a small current projection on existing jobs; no separate scheduler or camera-specific policy code is required. Implementation rationale and acceptance cases are in the [qualification plan](model-qualification-plan.md).

A prerequisite request may be active before a numerical candidate exists. It is keyed by subject, capability and evidence kind; the worker updates it as usable independent capture groups change and resolves it when that prerequisite is satisfied. It is advisory, not an assessment outcome or a promotion.

### 8.2 Experimental isolation

Resolve cohort/domain eligibility before constructing graphs, losses, uncertainty or weighting pools. Apply the existing group, serial and effective-contributor caps over the combined evidence entering each shared parameter block, not independently normalized body pools concatenated afterward.

Experimental fitting may consume pinned qualified references, but cannot update qualified coefficients, calibration, gauge designation, support history, native warm starts, contributor noise or scientific reputation/consistency state used by qualified weights. This boundary applies even when direct experimental Look weight is zero. Model-based diagnostics in unqualified domains remain separately identified; independent intake validation and audited human trust decisions remain applicable.

Only admitted native evidence defines the qualified body–Look graph. Recipe stages consume frozen targets and retain their existing allowed realization updates; they cannot seed a qualified native stage or alter its component graph. Do not relabel native images as recipes to reuse that boundary. Persist gauges independently of pool membership; ordinary admission preserves the reference, and WB-reference changes retain the audited refit/re-anchor policy ([02 §2.1](02-color-pipeline.md#21-exposure-and-wb-gauges)).

### 8.3 Assessment lifecycle and invalidation

Keep an immutable evidence snapshot and queryable dependency links for each report, with a separate current applicability/selection projection. A pending or failed execution is not a failed camera assessment. Preserve the last good eligible revision while a replacement is assessed.

| Event | Required transition |
|---|---|
| Eligible evidence or approved resources arrive | Dirty affected scopes, coalesce assessment and generate specific evidence requests |
| Applicable assessment and release gates pass | Select the exact candidate automatically within its assessed domain |
| Replacement fails or does not converge | Keep its diagnostics/resumable state as applicable; preserve a valid predecessor |
| New observations contradict the selected model | Assess the selected model as well as a replacement; replacement failure alone does not invalidate it |
| Withdrawal, exclusion, identity merge or qualifying evidence loss | Immediately remove affected current qualification assertions, recompute independence/weights and reassess, even with an empty fitting pool |
| Root revocation | Apply audited trust invalidation and invalidate dependent reports, including relevant transitive LUT dependencies |
| Applicable schema/epoch/profile/model/rule reinterpretation | Invalidate affected current assertions and commits; retain historical reports under their original identities |
| Append-only evidence during a valid run | Queue a successor without rejecting an otherwise valid snapshot or rolling back a newer selection |
| Lease loss or invalidated inputs | Reject affected selection and retain a durable successor request |
| Unrelated evidence | Leave unchanged assertions current |

Pending work and input invalidation are different generations. An append that preserves a snapshot's authorization, independence, weighting validity and interpretation need not fence its commit. Lease ownership, invalidating dependencies and selected-predecessor identity are checked transactionally. Events arriving during a run survive through the existing successor mechanism.

A selected target update schedules dependent realization assessment/refits; it does not retarget old calibration or CRM artifacts through a latest-version lookup. Old realizations retain their exact target pins and historical qualification. New qualified selections require compatible current reports; revocation or reinterpretation removes affected current claims before recomputation completes.

Known same-RAW re-renders stay together across training/assessment and do not count as independent captures. Client-claimed group IDs cannot establish independence between separate uploads. Mean-response evidence retains each body's systematic discrepancy before aggregation; noisy JPEG scatter is not latent truth ([02 §5](02-color-pipeline.md#5-uncertainty)). No RAW identity mechanism or new payload field is introduced.

Keep existing support hysteresis and gate lineage, without new repeated-pass counters or clock-based qualification expiry. Reports and invalidation notices are published through the authenticated publication root. Later qualification of unchanged bytes creates a new report/current association, not a modified old manifest, bundle or card. Historic replay shows current applicability under existing withdrawal policy; missing legacy reports never default to qualified.

## 9. Anti-spam: proof-of-work

- One proof-of-work per upload **batch**, bound to `H(server_nonce ‖ batch_manifest_hash)`, with **cost proportional to the number of images**. The nonce is issued once per send; the per-batch challenge keeps one batch's solutions from paying for a sibling. Proposals carry one too.
- **Memory-hard** (Argon2id-based, 16 MB *(tentative)*) to narrow the native/GPU advantage.
- **Split into k = 16 sub-puzzles** of equal expected cost. One puzzle's work is exponentially distributed (p95 ≈ 3× the mean); 16 sub-puzzles make waits predictable for photographers.
- **Difficulty:** sub-puzzle `j` needs `d = round(difficulty_bits_per_image + log2(n_images) − log2(16))` leading zero bits of `Argon2id(password = H(nonce ‖ batch_manifest_hash) ‖ j ‖ x, salt = "looklab-pow-v1")`, so expected work is `n_images · 2^difficulty_bits_per_image` evaluations. Defaults: 16 MiB memory, 1 iteration, `difficulty_bits_per_image = 8` (about 256 evaluations, a few seconds, per image; verification is one evaluation per sub-puzzle).
- **Server cost:** verification is k Argon2 evaluations. Do the cheap checks first, so junk costs a lookup, not an Argon2 run:
  1. request size limit;
  2. send instance (the nonce at its fixed offset in the envelope header, [03 §2.3](03-data-model.md)): for a new send, consume the nonce and publish the send binding in one short transaction after step 3; for a continuation, match the send manifest hash, credential id and assertion digest, reject a completed or expired send, and reject a batch that already arrived;
  3. WebAuthn signature and assertion counter, once when the send is established; continuations do not re-run them;
  4. one in-flight attempt per pending batch, then its sub-puzzles in order, rejecting at the first failure;
  5. strict parse of the full envelope and blobs. A failed post-PoW attempt counts against a small per-batch budget (three), so one solved puzzle cannot be replayed indefinitely against Argon2.

  Rate-limit nonce issuance and invalid submissions in memory. Keep the memory parameter small enough that a full verification stays well under a second.
- **Expiry and partial sends:** an unused nonce expires with the nonce row. Once a send is established, its `expires_at` is copied from that row and never extended; continuations read only the send row, so nonce-table cleanup cannot affect them. Accepted batches of an expired or abandoned send stay; the client starts a new send with the batches it has not completed.
- **Passkey registration** is otherwise unprotected. Hold registration challenges in memory with a short TTL, and **create the credential row only when its first valid upload arrives** (signature and proof-of-work both pass).
- **Target cost:** a few seconds per image on a mid-range laptop. Decoding already takes several seconds per image, so this adds only a fraction to processing time.
- **Difficulty is published** on the about page and scales down with tier and reputation.
- **Scope:** this deters scripted floods. It isn't a Sybil defense; caps are (§6).
- Per-credential rate limits are in memory. No IP addresses are stored.

## 10. Red-team harness (required, CI)

`looklab-trust` separates targeted policy checks, compact real fitter-to-trust integration, and full-scale contributor simulations. The full-scale scenario criteria below are unchanged; they are not the workload of ordinary CI.

| Scenario | Pass criterion *(initial)* |
|---|---|
| Honest-only, including a cohort shooting the same recipe on the same body | No honest image is down-weighted except by consistency checks; same-recipe cohorts aren't penalized |
| Many new-tier credentials shift one hue band (sized up to 10³ credentials and 10× the honest image count) | Published shift within each probe's gate bound; < 1.0 ΔE wherever σ_anchor ≤ 0.5 |
| Slow drift across ≥ 10 releases | Cumulative shift within the anchor bound; < 1.0 ΔE wherever σ_anchor ≤ 0.5 |
| Honest data arriving after a thin first rooted version | Gates pass without manual re-anchoring in ≥ 95% of releases |
| Concentrated shift of one hue's high-chroma probes (inside the 2% allowance), repeated every release | Blocked at the absolute ceiling; cumulative shift ≤ 3.0 ΔE from the anchor |
| Conflicting uploads that inflate a region's σ until it's unsupported | The supported→unsupported transition fails the gate and opens a moderation item |
| A body joins a component, with no real Look change | No gauge change; gates pass with no spurious drift |
| Two components merge | Exposure gauge converted exactly and gates pass; WB reference change handled by refit + moderator-approved re-anchor |
| Reputation farming with agreeing synthetic data, then a shift | Shift within what the new-tier cap allows; < 1.0 ΔE in rooted regions |
| Fake chart claims from new-tier credentials on one body | Input-transform change within the body-level cap; no gauge movement |
| Victim's serial reused by attacker credentials | The victim's weight drops by no more than the §2 cap order allows; reuse flag raised |
| Mislabeled images | ≥ 95% quarantined |
| Rollback | Revert + reject named contributions restores the pre-attack version within one release cycle; the contributor's other contributions remain weighted |

The two gauge rows (a body joining a component; two components merging) are asserted against the component and reference gauges in `looklab-fit` ([09 P4.2](09-roadmap.md)): a join keeps the designated body and its reference, a merge converts the exposure gauge exactly through `ExposureGauge::between`, and a WB reference change takes `WbGaugeChange::RefitAndReanchor`.

| Validation tier | Execution policy |
|---|---|
| Targeted policy and numerical regression tests | Required on pushes and pull requests; use public policy inputs and compact synthetic fixtures |
| Compact real fitter-to-trust integration | Required before releases through **Pre-release integration**, not ordinary CI; measured job wall time exceeded the approximately two-minute budget |
| Worker fitter/publication integration | Required before releases through **Pre-release integration**; native/consistency fits, LUT rebasing and match-bundle publication require `looklab-worker/integration` |
| Full-scale simulations, numerical studies and benchmarks | Explicit maintainer request only; no automatic push, pull-request, scheduled or release execution |

Targeted cases must retain the behavior that distinguishes a valid honest contribution from an attack. Contributor-count boundaries and repeated anchor comparisons remain tested without refitting a growing image pool. Compact integration uses the real fitter, uncertainty estimates and release gates; it must not pass through unsupported fits or blanket rejection.

Run targeted trust checks with `nix develop -c cargo nextest run --locked --release -p looklab-trust --features harness --no-fail-fast --ignore-default-filter`. This includes the pure gauge cases; the compact binary requires the separate `integration` feature. Run compact integration with `nix develop -c cargo nextest run --locked --release -p looklab-trust --features integration --test compact --no-fail-fast --ignore-default-filter`.

Worker ordinary CI retains ten fit-free or budget-short-circuit cases, plus the separate supersession coverage. Forty fitter-dependent cases retain their fixtures and assertions in the pre-release tier, including publication, quarantine, anchor movement, revert/rejection, recipe isolation, artifact integrity, firmware-split evidence, LUT rebasing and match-bundle publication. Run that tier with `nix develop -c cargo nextest run --locked --release -p looklab-worker --features integration --test worker --test lut_rebase --test match_bundle --no-fail-fast --ignore-default-filter`; the ten inexpensive cases also run under that feature. The 1,000-image timing case and ignored fixture-convergence diagnostic require the separate `full-scale` feature and are not part of pre-release validation.

The compact contract performs one noiseless colorimetric fit over three 500-sample images with production fitting defaults, verifies a supported mid-gray probe and publication, then checks the anchor ceiling against a constructed persisted-history mismatch. It does not claim to reproduce the full-scale attack populations. Before publishing a release, push its `v*` candidate tag or manually dispatch **Pre-release integration** for that commit and require a passing result. No full-scale feature is enabled by this workflow.

Maintainers can request the **Full-scale validation** workflow manually. Its local equivalent is `nix develop -c cargo nextest run --locked --release -p looklab-fit -p looklab-trust -p looklab-worker --features looklab-fit/full-scale,looklab-trust/full-scale,looklab-worker/full-scale --run-ignored all --no-fail-fast --ignore-default-filter`. This includes the opt-in diagnostic and timing cases and the 1,000-image worker benchmark. The features select the original full-scale assertions without changing production fitting defaults or gate thresholds. Agents may suggest this run with a description of the affected behavior, but may not launch it without the maintainer's request.

## 11. Audit log and artifact integrity

- A plain **append-only table** (`audit_log`), published as a page and as JSON on the static site. Entries cover:
  - grants and revocations;
  - endorsements;
  - root admissions;
  - moderation decisions;
  - contribution rejections and future-intake blocks;
  - re-anchors and reverts;
  - epoch/look splits;
  - Look hiding, removal and relisting;
  - release manifests.
- Artifacts are **content-addressed** ([03 §4](03-data-model.md)). Each version manifest lists its artifact hashes, and clients check downloaded artifacts against the manifest.
- Qualification reports and current applicability notices are hash-bound through the publication root. Private evidence/dependency records remain private; public reports expose permitted aggregates, reasons and limitations, not sample contents, RAW identifiers, credential keys or serial HMACs.
- Entries reference credentials by internal id or display handle, never by public key or serial HMAC. Historical revert entries keep their original credential-exclusion payload and render as previous-policy exclusions; they are not rewritten into contribution ids.
- The server publishes the log at `GET /log.json` (paged with `after` and `limit`), the grant and endorsement lists at `GET /lists/grants.json` and `GET /lists/endorsements.json` (paged the same way, capped at 100 records), and each version manifest at `GET /manifests/<look>/<version>.json`. Both lists render scope rules as plain-language text alongside the rule list.

## 12. Recipe channel and bounties

- Recipe data **never** enters any Look's θ_L, not just the Look it attempts. It updates only body parameters and the control-response model, in the recipe stage of the fit ([02 §4.3](02-color-pipeline.md)), subject to the body-level cap.
- **Scoring.** A recipe score uses `ScoringMetric::RenderedPostClipV1` (`looklab-scoring-metric/rendered-post-clip/1`) and its `OutputContext { target, camera }` ([02 §5](02-color-pipeline.md#5-uncertainty), [03 §1](03-data-model.md#rendered-metric-output-context-and-assessment-status)). The target is rendered through its pinned reference encoding and clip, the prediction through the selected output encoding and the realization clip, and the captured JPEG is decoded once into the common CAM16-UCS frame. Exposure compensation and WB adjustments move the prediction, not the target; they cannot change membership or re-bin colours, and a capture-specific nuisance correction is frozen, not refitted per recipe. Signed residuals are retained separately from the descriptive raw gap. Scores and residual observations group by the actual camera output context, so identical controls with different output encoding or realization clip do not merge; output encoding is not assumed to be a schema control. A historical score with `LegacyPreClipV1` keeps its pre-clip meaning and is not compared with a rendered score; it records the old pre-clip target/prediction comparison against decoded post-clip JPEG, not inverse clipping of old observations. Old raw-gap and pixel-SEM results are not calibrated systematic evidence under the rendered policy. The current artifacts do not establish calibrated arbitrary-colour joint uncertainty, so a significance that depends on it stays `AssessmentStatus::Unassessed`.
- **Persistent-residual analysis** per (Look, body) after each release:
  1. The estimand is the **systematic conditional-mean excess**, not a positive raw gap. Even a correct mean-response model has positive expected JPEG distance from observation noise, and averaging captures does not remove that floor from the mean of distances. Estimate signed residuals within the declared colour/settings cells, then aggregate with fixed weights; do not average unrelated colours or subtract the fitted contributor noise scale. The fixed population is a declared `RegionConditionScope` ([03 §1](03-data-model.md#rendered-metric-output-context-and-assessment-status)): duplicate or invalid scopes are rejected, a single colour does not stand for the whole Region unless the scope declares only that colour, and missing cross-colour pooled σ leaves the aggregate `Unassessed` and not persistent. `systematic_sigma` stays `None` unless the full regional assessment is `Assessed`, which requires per-condition joint moments and the declared aggregate σ; `region_uncertainties` never falls back to `observation_sigma`, because scatter is not model-mean or regional-gap uncertainty.
  2. A region is a residual-bounty candidate only when the conditional-mean excess is assessed and persistent and a `ReachabilityStatus::Reachable` witness exists. A region without that witness stays unassessed for residual purposes and does not become a residual bounty. A bounty may instead request data for an assessed coverage gap; missing support alone is not a residual claim. The existing `>2σ` and five-effective-contributor rule is not permission to apply pixel SEM around a noise-biased raw gap. Where the evidence cannot distinguish the conditional mean from observation scatter, significance stays **unassessed** and no residual bounty is created.
  3. Attribution and reachability are separate questions: parameter uncertainty is not freedom to move camera controls. Attribute the signed conditional-mean camera residual `μ_o − m` to three non-overlapping covariance-scaled groups: the shared camera-to-scene input transform, the realization/control model (CRM, body exposure/WB offsets, firmware adjustments and realization clip), or the Look's canonical base-family coefficients. Freeze the physical gauge, use valid white-preserving coordinates, measure perturbations in joint-covariance units rather than raw coefficient units, and for a component-only perturbation hold the other groups fixed and restrict the displacement to the identifiable range of the joint covariance. Project the induced response into the same post-clip residual frame, using common residual-covariance whitening when available. Preserve perturbation amplitude; unit-vector normalization compares angles, it does not erase explanatory strength. Check the local approximation with feasible forward perturbations, without refitting or changing stored parameters. A group may span several response directions; there is no single all-coefficients bump and no largest normalized scalar derivative.
  4. Overlapping explanations are **Mixed**. Missing covariance, unidentified directions or invalid perturbations are **Unassessed**. These labels describe compatibility, not proven causation. A region-level bounty can retain unassessed attribution without naming a cause. A Look-compatible discrepancy may justify a request for independent native/reference evidence; recipe evidence never retrains the requested target.
  5. Resolve the body's target through the selected family version or the eligible LUT composition and record the exact input-transform, control-response, target, source realization and body-state versions. A published but unselected version is never analyzed, and a missing exact binding produces no observation.
- **Reachability and the settings witness.** `REACHABILITY_TOLERANCE_DE = 2.0` is applied to the rendered metric. A `ReachabilityStatus::Reachable` claim requires one legal settings vector in `Reachability.witness`, exact post-clip evaluation against the frozen target, error at most the tolerance at **every assessed point in the declared assessment set**, and applicable model support and settings/domain evidence. `Reachability.declared` is the declared set; `evaluated` and `within_tolerance` are descriptive rendered results; `assessed` and `within_assessed` count only points with an applicable target and realization/settings-domain witness, and `within_assessed` drives `status`. Without that domain, retain `declared` and all raw `evaluated`/`within_tolerance` counts, set `assessed = 0` and `within_assessed = 0`; a raw descriptive miss may still report `NoRecipeFound` (not unreachable), while all raw points within tolerance without domain report `Unassessed`. Only `within_assessed` can justify `Partial` or `Reachable` under applicable support. Missing target uncertainty or support leaves confidence `Unassessed`, not fully supported. Different settings reaching different probes do not establish one usable recipe, and unsupported points do not disappear from the denominator. Partial coverage is `ReachabilityStatus::Partial`; absent evidence is `Unassessed`. Exposure×hue membership uses the original scene input, physical exposure and input hue, not output hue or recipe-adjusted coordinates; neutral stays a category and is not hue zero. The neutral recipe category is distinct from the global 12 hue × 6 exposure trust caps; no cap or rate change follows. A heuristic search failure is `ReachabilityStatus::NoRecipeFound`; no `Unreachable` status is emitted, and proving unreachability requires a valid bound or exhaustive coverage of the declared legal candidate space. For recipe-residual analysis, the recipe's own settings are its witness, not another recipe's regional optimum. These are model-relative claims with pinned output encoding and uncertainty, not promises about every future JPEG; no exhaustive solver is required.
- **Bounties:** rank data requests by expected reduction in uncertainty-weighted user error; publish the top N. Scoped assessment failures or missing evidence produce named sim/settings/combinations, capture, colour/exposure or calibration-source requests, not routine moderator promotion tasks. A bounty can't be marked resolved by new-tier data alone. A missing producer joint σ or observation noise creates no variance-reduction bounty; separate assessed coverage and evidence requests remain. Unassessed off-grid support, pixel SEM, a nearby probe's support, an assumed independence or a synthetic calibration/engine check cannot substitute for the missing evidence; missing assessments do not block rendering, contribution intake or existing valid probe-based matching and card paths.

## 13. Cut at this scale

These were considered and deliberately left out, to be revisited if the threat level changes:
- Similarity-based Sybil detection (FoolsGold) and root-agreement scoring. Honest same-recipe cohorts look like Sybils, and the agreement score was hard to calibrate.
- Server re-verification of client-claimed kinds.
- Browser-stored plain keys.
- Upload quotas.
- OPRF for serials.
- Privacy Pass.
- Signed or hash-chained logs.
- A repo trust file with CODEOWNERS (grants live in the database + audit log).
- Byte-level duplicate flags.
