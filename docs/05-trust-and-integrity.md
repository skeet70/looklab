# 05 — Trust and integrity

**Goal (hobby scale):** reasonable contribution weighting, transparent anti-spam and a low moderation load, with no user accounts.

**Non-goal:** resisting a determined, patient attacker. Such an attacker can nudge regions that are already labeled provisional by a bounded amount. Caps bound the damage, release gates stop it accumulating, and **cheap rollback** repairs it (§8). We don't try to prevent it outright.

**Design in one paragraph:**
- **Identity:** passkeys, only for contributing.
- **Weight:** image weight = kind × evidence × tier × reputation, then caps.
- **Trust:** only a human endorsement moves a contributor out of the capped new tier. Reputation nominates contributors for endorsement but never grants trust by itself.
- **Serial merging:** can only *reduce* weight, through caps.
- **Releases:** gated against the previous version and cumulatively against a frozen anchor, with a one-click revert.

## 1. Threat model

- The client is open source and can be modified, so **contribution kinds (chart, pattern, gradient, re-render) are client-claimed**. At this scale that's acceptable: a false claim from a new-tier credential only shifts weight *within* the capped new-tier pool (§6). Server re-verification of kinds can be added later if needed.
- The published models (Looks, input transforms, control-response models) let anyone synthesize data that agrees with them. That can farm reputation (§4), but reputation can't lift a credential out of the new tier, so the effect is bounded by the cap.
- Honest mistakes (wrong sim, auto DR, edited JPEGs) are handled by the per-image consistency checks ([02 §4.4](02-color-pipeline.md)). They apply to every tier, seeders included.

## 2. Identity

- **Passkeys (WebAuthn) only, and only for contributing.** Browsing, matching, previews and the verification loop are anonymous and need no credential. Server side: `webauthn-rs`.
- **Platforms without a built-in passkey store** (common on Linux desktops) can use a phone via the cross-device QR flow, a security key, or a password manager that stores passkeys. The contribute page says so up front.
- **Signing:** each upload batch is signed by a passkey assertion whose challenge is `H(server_nonce ‖ batch_manifest_hash)`. Withdrawal/deletion requests are signed the same way.
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
- After each release in which the contributor had data:
  ```
  r ← clip(r + η·(a − 0.5) − penalties, 0.2, 1.5),   η = 0.1
  ```
  - `a` = the fraction of the contributor's images in that release accepted without down-weighting.
  - Penalties: 0.2 per quarantine judged deliberate by a moderator, 0.05 per automatic quarantine.
  - Bonus: +0.05 per chart session that passes consistency.
  - Decay toward 0.5 with a 1-year half-life.
- **Reputation raises weight only within a tier.** It never moves a credential out of the new tier and never bypasses the group cap. Farming it with synthetic data is therefore bounded.
- **Reputation nominates; people decide.** High-reputation new-tier contributors appear in seeders' endorsement queue.
- Display: a coarse tier/badge plus accepted-contribution counts.

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
- Per-image consistency checks quarantine or down-weight images regardless of tier.
- **Duplicates (honest):** images from the same effective contributor with the same capture-metadata tuple are merged into one weight. Shuffling makes uploads byte-different, so there's no byte-level comparison.
- **Chart anchoring of input transforms:** only chart sessions from `endorsed` or `seeder` credentials enter as gauge anchors with tight priors ([02 §2](02-color-pipeline.md)). New-tier chart sessions count as ordinary data, subject to the body-level cap (§6).

## 6. Caps

- **New-tier group cap:** all new-tier data together gets at most **γ = 30%** *(tentative)* of the weight:
  - per Look, **per region** (12 hue × 6 exposure cells over supported probes);
  - per body, for the body parameters (input transform, ξ, ω, control-response model), because a body update moves every Look on that body.

  In regions or bodies with no endorsed or root data, new-tier data defines the value, and the region/body stays **provisional**.
- **Per effective contributor:** with `n` effective contributors in the pool, at most `max(0.05, 2/n)` of a Look version's weight and `max(0.20, 2/n)` of a body input transform's weight, applied by water-filling against the final total alongside the group cap. Diminishing returns `1/sqrt(1 + n/20)` also apply on the n-th image of one contributor. The floors and the factor 2 are tentative ([10 Q12](10-open-questions.md)).
- **Minimum pool (provisional label only):** a Look or body is labeled provisional when its pool has fewer than 20 (Look) or 5 (body) effective contributors and it is not rooted (§7). The threshold does not switch the per-contributor caps off.
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

## 8. Release gates and rollback

**Anchor:** the first rooted version of a Look, or the last version a moderator explicitly re-anchored (the re-anchor action requires a reason, e.g. a new root session). Re-anchoring is needed so that legitimate long-term improvement isn't blocked forever.

**Gated set.** The gated probes are the **sticky union**: every probe supported in any version published since the current anchor, plus every probe in either compared version's supported set. A probe that loses support for any cause stays gated until a re-anchor, which restarts the union from the new anchor's supported probes. So making a region uncertain, thin or biased cannot take it out of the gate.

**Support.** Each probe carries a pre-clip prediction (the `J'a'b'` the Look defines) and a post-clip prediction in the version's reference output encoding (pre-clip `J'a'b'` → output RGB → soft clip → `J'a'b'`), each with its own σ, and the identity-prior-attributable bias in ΔE. A probe is supported when all four causes hold:

- post-clip `σ ≤ 1.5` entering and `σ > 1.65` leaving;
- its bias is at most `0.33 × 1.5` ΔE entering and above `1.1 ×` that leaving;
- its region reaches the absolute density floor (tempered effective weight per bin, not a share of the mean bin) and it has at least 3 distinct-image clusters. Neither has hysteresis.

Enter and exit thresholds differ so that honest boundary jitter does not flip a probe between versions, and the previous published version's per-cause state is an input to marking. The first version uses the enter rule only. The absolute floor means data uploaded elsewhere cannot push a rare region below it.

A candidate LookVersion is published only if:

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

A failed gate leaves the candidate provisional and opens a moderation item. A native fit that does not converge never reaches the gate: nothing is published, the worker resumes that Look from the stored fit state in the next batch, and 3 consecutive non-converged batches *(tentative)* open one "Fit not converging" moderation item, resolved by the next published version.

**Rebase.** Each published `lut_defined` version records its declared base in `lut_base_pins`. Pins reject self-edges and cycle-closing Look dependencies before they are recorded, so no pin creates an automatic rebase loop. A `family_rendering` base that declares settings, for example `dynamic_range: dr400`, stays pending; it is not pinned to a bare native `look@version` unless that native release demonstrably carries those settings. A base publish, a family-base resolution change, or a revoked base root writes `lut_rebase_pending` and enqueues a `lut_rebase` job, which the worker consumes without waiting for a nightly image batch. The candidate re-imports the stored LUT against the resolved `look@version` base document, not the neutral preview, and inherits the base's uncertainty and support through the importer. It is gated against the Look's first published version or last re-anchor with the existing `gate_release` inputs; root observations are empty for LUT Looks. Only a passing candidate gets a manifest and artifacts. A failed gate writes a `gate_failures` row, the existing moderation item, and leaves the previous version's bytes and matchability untouched. Pins are never re-anchored automatically. A revoked root's audit payload gains `lut_dependents: [{look, version}]`, including transitive LUT dependents, and `RootRevocation` exposes the same list; `lut_dependents` is empty when no LUT was pinned to the affected base versions.

**Rollback:** one moderator action, "revert to version N, exclude credential(s) K, refit". The revert publishes N's content as a new version immediately, then schedules the refit without K. Published versions stay immutable and pinnable. A revert is not a re-anchor: the gated union continues across it.

**Publication.** A moderation hide, remove or relist records a publication request that survives the running publisher, and the publisher's successor is enqueued in the transaction that completes the current run.

**Release artifacts.** A version publishes its Look document (`look.looklab`), preview LUT, card file, recipe-stage control-response model and release record. Each is listed by hash in the version manifest. The version also carries one match bundle per body epoch in its release provenance ([03 §5](03-data-model.md)), verified through the same manifest. The worker assembles those bundles with the new version, before writing its manifest, from the realizations published at that moment plus the release's own in-memory candidate sim. A sim without a published realization then is listed as excluded, and a later sibling release does not add it to that bundle. A later source-sim release, schema update or input-transform update does not change the version's bytes or manifest. Listing state is applied when the site is written, so a release created while its Look is hidden still carries the bundle its own publication froze and becomes matchable when relisted. A version published before bundles were bound this way has no bundle and does not gain one retroactively.

## 9. Anti-spam: proof-of-work

- One proof-of-work per upload batch, bound to `H(server_nonce ‖ batch_manifest_hash)`, with **cost proportional to the number of images**. Proposals carry one too.
- **Memory-hard** (Argon2id-based, 16 MB *(tentative)*) to narrow the native/GPU advantage.
- **Split into k = 16 sub-puzzles** of equal expected cost. One puzzle's work is exponentially distributed (p95 ≈ 3× the mean); 16 sub-puzzles make waits predictable for photographers.
- **Difficulty:** sub-puzzle `j` needs `d = round(difficulty_bits_per_image + log2(n_images) − log2(16))` leading zero bits of `Argon2id(password = H(nonce ‖ manifest hash) ‖ j ‖ x, salt = "looklab-pow-v1")`, so expected work is `n_images · 2^difficulty_bits_per_image` evaluations. Defaults: 16 MiB memory, 1 iteration, `difficulty_bits_per_image = 8` (about 256 evaluations, a few seconds, per image; verification is one evaluation per sub-puzzle).
- **Server cost:** verification is k Argon2 evaluations. Do the cheap checks first, so junk costs a lookup, not an Argon2 run:
  1. request size limit;
  2. nonce (read at its fixed offset in the envelope header, [03 §2.3](03-data-model.md)) was issued by this server, is unused and unexpired (consume it);
  3. WebAuthn signature;
  4. sub-puzzles, in order, rejecting at the first failure;
  5. strict parse of the full envelope and blobs.

  Rate-limit nonce issuance and invalid submissions in memory. Keep the memory parameter small enough that a full verification stays well under a second.
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
| Rollback | Revert + exclude restores the pre-attack version within one release cycle |

The two gauge rows (a body joining a component; two components merging) are asserted against the component and reference gauges in `looklab-fit` ([09 P4.2](09-roadmap.md)): a join keeps the designated body and its reference, a merge converts the exposure gauge exactly through `ExposureGauge::between`, and a WB reference change takes `WbGaugeChange::RefitAndReanchor`.

| Validation tier | Execution policy |
|---|---|
| Targeted policy and numerical regression tests | Required on pushes and pull requests; use public policy inputs and compact synthetic fixtures |
| Compact real fitter-to-trust integration | Required before releases through **Pre-release integration**, not ordinary CI; measured job wall time exceeded the approximately two-minute budget |
| Worker fitter/publication integration | Required before releases through **Pre-release integration**; normal-budget native and consistency fits require `looklab-worker/integration` |
| Full-scale simulations, numerical studies and benchmarks | Explicit maintainer request only; no automatic push, pull-request, scheduled or release execution |

Targeted cases must retain the behavior that distinguishes a valid honest contribution from an attack. Contributor-count boundaries and repeated anchor comparisons remain tested without refitting a growing image pool. Compact integration uses the real fitter, uncertainty estimates and release gates; it must not pass through unsupported fits or blanket rejection.

Run targeted trust checks with `nix develop -c cargo nextest run --locked --release -p looklab-trust --features harness --no-fail-fast --ignore-default-filter`. This includes the pure gauge cases; the compact binary requires the separate `integration` feature. Run compact integration with `nix develop -c cargo nextest run --locked --release -p looklab-trust --features integration --test compact --no-fail-fast --ignore-default-filter`.

Worker ordinary CI retains nine fit-free or budget-short-circuit cases, plus the separate supersession coverage. Eighteen normal-budget fitter cases retain their fixtures and assertions in the pre-release tier, including publication, quarantine, anchor movement, revert/exclusion, recipe isolation, artifact integrity and firmware-split evidence. Run that tier with `nix develop -c cargo nextest run --locked --release -p looklab-worker --features integration --test worker --no-fail-fast --ignore-default-filter`; the nine inexpensive cases also run under that feature. The 1,000-image timing case requires the separate `full-scale` feature and is not part of pre-release validation.

The compact contract performs one noiseless colorimetric fit over three 500-sample images with production fitting defaults, verifies a supported mid-gray probe and publication, then checks the anchor ceiling against a constructed persisted-history mismatch. It does not claim to reproduce the full-scale attack populations. Before publishing a release, push its `v*` candidate tag or manually dispatch **Pre-release integration** for that commit and require a passing result. No full-scale feature is enabled by this workflow.

Maintainers can request the **Full-scale validation** workflow manually. Its local equivalent is `nix develop -c cargo nextest run --locked --release -p looklab-fit -p looklab-trust -p looklab-worker --features looklab-fit/full-scale,looklab-trust/full-scale,looklab-worker/full-scale --run-ignored all --no-fail-fast --ignore-default-filter`. This includes the opt-in diagnostic and timing cases and the 1,000-image worker benchmark. The features select the original full-scale assertions without changing production fitting defaults or gate thresholds. Agents may suggest this run with a description of the affected behavior, but may not launch it without the maintainer's request.

## 11. Audit log and artifact integrity

- A plain **append-only table** (`audit_log`), published as a page and as JSON on the static site. Entries cover:
  - grants and revocations;
  - endorsements;
  - root admissions;
  - moderation decisions;
  - re-anchors and reverts;
  - epoch/look splits;
  - Look hiding, removal and relisting;
  - release manifests.
- Artifacts are **content-addressed** ([03 §4](03-data-model.md)). Each version manifest lists its artifact hashes, and clients check downloaded artifacts against the manifest.
- Entries reference credentials by internal id or display handle, never by public key or serial HMAC.
- The server publishes the log at `GET /log.json` (paged with `after` and `limit`), the grant and endorsement lists at `GET /lists/grants.json` and `GET /lists/endorsements.json` (paged the same way, capped at 100 records), and each version manifest at `GET /manifests/<look>/<version>.json`. Both lists render scope rules as plain-language text alongside the rule list.

## 12. Recipe channel and bounties

- Recipe data **never** enters any Look's θ_L, not just the Look it attempts. It updates only body parameters and the control-response model, in the recipe stage of the fit ([02 §4.3](02-color-pipeline.md)), subject to the body-level cap.
- **Persistent-residual analysis** per (Look, body) after each release:
  1. Find regions where verified recipe outcomes exceed the prediction by > 2σ across ≥ 5 effective contributors, in regions the control model claims are reachable.
  2. Attribute the gap to the input transform, the control model or the Look.
- **Bounties:** rank data requests by expected reduction in uncertainty-weighted user error; publish the top N. A bounty can't be marked resolved by new-tier data alone.

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
