# Scoped camera models and automatic qualification

Status: reviewed implementation guidance for the canonical design in [docs/00–10](../README.md#design-docs), not a separate specification or a claim of numerical readiness. Canonical sections own the contracts; this plan retains integration rationale, source-inspection context and acceptance cases.

## 1. Decisions

- Keep broad contribution intake. Do not introduce a reference-camera allowlist for contributions or public experimental results.
- Complete the reference body's working fit, compare, match and export loop. Other bodies can have usable experimental results without individually satisfying the reference milestone.
- Use one common model, evaluator and fitting implementation. Learn camera-specific parameters as data, not camera-specific algorithms or bespoke CI projects.
- Preserve the intended sensor-independent Look target and camera-specific realizations. Complete that architecture's worker integration; do not replace it with a catalog of independently evolving per-camera Looks.
- Derive qualification from versioned checks on independent evidence. Do not maintain camera badges by hand, infer scientific validity from upload counts, or promote contributor trust through model agreement.
- Promote, invalidate, refit and request missing evidence through the existing worker. A camera owner supplies measurements, not a maintainer-written integration for each camera.
- Retain the current numerical correctness, convergence, mean-response uncertainty, support, privacy and release-safety contracts. Experimental status is not permission to publish an unconverged or gate-failing model.

The reference milestone establishes an end-to-end baseline, not exclusive camera support. Qualification is a scoped claim about particular behaviour and dependencies, not a binary statement that every mode of a body is correct.

The common Look is a function in the shared scene/appearance domain. Camera input calibration maps sensor RGB into that domain; firmware/control response and output rendering describe the camera's realization. Matching searches a camera's available realizations for an approximation to the same target. Camera-specific calibration is not a second canonical Look applied after the first.

## 2. Current implementation and gaps

The following records the inspected baseline that motivated the handoff, not runtime qualification or a live implementation inventory. Reread concurrent source changes before acting.

| Concern | Observed implementation | Required change |
|---|---|---|
| Common representation | [docs/02](02-color-pipeline.md) specifies shared Look layers with body input transforms, WB/exposure offsets, realization clipping, firmware offsets and control responses | Keep coefficient/evaluator semantics common across bodies and qualification levels |
| Worker fitting | [`refit::run` and `refit_look`](../crates/looklab-worker/src/refit.rs) fit each body independently | Replace the independent-target publication path with an explicit family-target stage and camera-realization stages; retain independent fits as diagnostic evidence |
| Hierarchical library | [`fit_hierarchical`](../crates/looklab-fit/src/hierarchical.rs) exists, but the worker does not call it | Preserve per-image illuminant transforms and realization identities during integration; existence of the entry point does not prove convergence |
| Realization scope | [`pool::build`](../crates/looklab-worker/src/pool.rs) groups by body and Look; [`roots::base_sim`](../crates/looklab-worker/src/roots.rs) selects an epoch/sim from native evidence | Partition by resolved epoch and native sim before selecting schema or fitting |
| Release identity | [`look_versions`](../crates/looklab-server/migrations/0002_scope_rules.sql), [`fit_attempts`](../crates/looklab-server/migrations/0012_fit_attempts.sql) and several worker selectors are Look-wide | Isolate histories, attempts, holds, fingerprints and selections by model subject |
| Published selection | [`LookPage::latest`](../crates/looklab-publish/src/site.rs), worker publication and bundle selection use published/rooted versions, not empirical qualification | Add explicit scoped selectors; maximum version is not a scientific selection policy |
| Camera readiness | [`PreparationPage`](../crates/looklab-publish/src/body.rs) reports input-transform availability | Keep resource readiness separate from measured behaviour and contributor/root authority |
| Uncertainty evidence | [`model_error`](../crates/looklab-fit/src/model_error.rs) performs held-out assessment with conditional nuisance handling | Preserve evidence identity and limitations through the worker and publication boundary |
| Resources | [`TransformDir`](../crates/looklab-worker/src/body.rs) loads server-side transform files | Resolve approved, pinned calibration sources automatically where available; do not assume local DNG matrices reach server authority |
| Scheduling and integrity | Existing durable jobs, generation handling, immutable artifacts, manifests and publication roots | Extend these mechanisms rather than adding another scheduler or status database |

Two bodies' blobs can coexist today without replacing bytes. They can still interfere through the Look-wide previous release, anchor, attempt state and latest selector. This is a semantic identity problem, not merely a filename collision.

## 3. Model subjects and artifact identity

Use a closed, typed model-subject identity:

- `SharedFamily(family)`: the canonical sensor-independent target, with one stable family identity and immutable coefficient versions. Its estimate can be provisional, source-limited or cross-body qualified.
- `BodyRealization(body, epoch, native_sim)`: camera-specific parameters and control responses bound to an exact family target version. This is not another Look target.

The schema content hash, firmware interpretation, profile, gauge/component identity and cohort revision are dependencies of a subject's candidate, not a reason to rename that subject on every update. An epoch split creates a new realization scope. A cohort membership update does not reset a shared family's anchor or supported-probe history.

A family candidate binds immutable Look coefficients and their evidence/dependencies. A realization candidate binds a pinned family version, body calibration, realization parameters and CRM. Realization versions can change without redefining the family target. Keep the existing Look/CRM coefficient representation and family `look/version` locators; give realization revisions their own explicit identity. No new Look kind is needed for each camera.

Camera-bundle snapshots have an independent immutable `MatchBundleRevision` identity and manifest for `(body, epoch)`. Each source sim pins its own native-family realization; the requested target is separate. This lets a new camera/realization become usable against an unchanged target without rewriting an old target manifest or publishing an artificial target version. Historical target-owned bundles remain resolvable.

When a family has no target, eligible native evidence from any supported body may seed its single provisional canonical estimate under existing trust, resource and persisted gauge policy. Its explicit seed cohort can continue refitting as eligible data arrives; additional experimental bodies consume pinned targets until separately admitted. This keeps a source-limited target improving without silently pooling unexamined cameras. Seeding grants neither root authority nor empirical proof of sensor independence. Independent camera-local Look fits remain diagnostics, not competing canonical targets. The canonical identity/bootstrap contracts are in [03 §1](03-data-model.md#1-entities) and [05 §7](05-trust-and-integrity.md#7-root-set).

Carry subject identity through pool construction, version allocation, previous/anchor lookup, support history, fit attempts, resume state, holds, fingerprints, realization parameters, reports, manifests, bundles, cache keys, permalinks and current selectors. A body-only filename or report annotation is insufficient. Two sims mapped to the same family, or two epochs on one body, must not share resumable state or silently adopt the first image's schema.

Map stable family, firmware and effective-contributor identities consistently within a shared invocation. Body-local ordinals are not globally unique firmware identities.

Preserve old manifests and model bytes. Backfill a legacy artifact's scope only where retained provenance identifies it unambiguously. Unknown legacy scope is explicitly unassessed, not a shared-family model or a qualified camera. Historical pins remain reproducible under their existing publication policy; they do not acquire new qualification by migration.

New publication metadata may require a schema revision and a coordinated consumer cutover. Qualification alone does not require changing Look coefficients, the evaluator or `card.llk` semantics. Do not rewrite immutable manifests to attach later qualification.

## 4. Independent state axes

The state definitions and current projections are owned by [03 §1](03-data-model.md#1-entities); eligibility and public interpretation are owned by [05 §8.1](05-trust-and-integrity.md#81-scoped-qualification) and [08](08-ui.md). Keep preparation, numerical validity, empirical qualification, publication and rooted trust distinct at every integration boundary. A single body-wide boolean or overloaded provisional status cannot represent them.

## 5. Three qualification capabilities

The common versioned protocols are specified in [02 §5.1](02-color-pipeline.md#51-qualification-claims-and-protocols), with their existing acceptance values in [09](09-roadmap.md). Implement them once rather than adding per-camera test programs or a policy DSL.

### 5.1 Native prediction

Use the canonical independent mean-response discrepancy and support evidence in [02 §5](02-color-pipeline.md#5-uncertainty). This supports a conditional camera response in the assessed domain, not absolute calibration or future-JPEG scatter. Preserve evidence availability and scope through the fitter/worker boundary.

### 5.2 Controls and matching

Use the canonical [realization and matching contract](04-matching-and-export.md#2-realization-model-on-a-body-stock-firmware). Preserve the P2.2 observed-JPEG and P2.3 settings-recovery estimands. Do not turn interpolation, defaults or independent single-control checks into empirical evidence for untested settings/combinations.

### 5.3 Cross-body pooling

Use the canonical [hierarchical fit](02-color-pipeline.md#43-global-hierarchical-fit-multi-body) and [admission policy](05-trust-and-integrity.md#81-scoped-qualification). P4.3 qualifies the engine version using known-reference evidence; it does not require unavailable latent truth for every incoming camera. Camera/domain admission uses independent scoped agreement/discrepancy evidence and schedules a new family fit, rather than copying an independent camera diagnostic target. Q18 remains an empirical premise.

## 6. Isolation before computation

The normative influence and stage boundaries are in [05 §8.2](05-trust-and-integrity.md#82-experimental-isolation) and [02 §4.3](02-color-pipeline.md#43-global-hierarchical-fit-multi-body). Enforce them before graph, loss, contributor-noise and normalized weight construction, not just at publication. Test indirect paths through gauges, reputation and warm starts as well as direct Look weights.

The inspected hierarchical interface's single CRM/clip state per body and D65-only transform uses require reconciliation with the canonical realization-keyed parameters and per-image illuminant handling. This is an integration requirement, not authorization for a solver or colour-model rewrite.

## 7. Resources and broad camera support

Canonical profile provenance and source authority are specified in [02 §2](02-color-pipeline.md#2-body-input-transform-a_ct) and [03 InputTransform](03-data-model.md#inputtransform). The inspected upload header does not transmit arbitrary local RAW-derived matrices/provenance. Reuse approved pinned server-side sources and existing proposal paths; explicit missing-source blockers are preferable to invented calibration or a new upload contract.

## 8. Reports, dependencies and automatic lifecycle

Canonical report, dependency and current-selection records are in [03 §1](03-data-model.md#1-entities). Lifecycle transitions, invalidation and publication fencing are owned by [05 §8.3](05-trust-and-integrity.md#83-assessment-lifecycle-and-invalidation).

Extend existing jobs, version storage and publication requests. Preserve queryable private dependencies as well as content digests. Separate append-only pending work from input invalidation so continuous uploads do not starve valid commits; selected-predecessor checks prevent an older run from rolling back a newer result. Missing evidence requests and invalidation notices must survive jobs that produce no new model artifact.

## 9. Publication and consumer selection

Canonical artifact associations and historical immutability are in [03](03-data-model.md); consumer selection, downloads and pinned replay are in [04](04-matching-and-export.md) and [08](08-ui.md). Update these consumers together. Experimental realization publication cannot redefine the family target, imply qualification or bypass numerical/gate failures.

Keep qualification provenance outside unchanged card bytes. Later qualification creates a new report/current association, not a rewritten manifest, bundle or card. Pin immutable selection mode and assessment references for the target and full ranked source context in permalinks; query current applicability separately. Body-agnostic card downloads do not require an unused camera CRM. Missing-evidence requests should use existing bounty machinery rather than a routine camera-promotion moderation queue.

## 10. Delivery order and release boundary

1. Specify family and realization identities, scoped claims and rule semantics; add subject-bound storage and migrate unambiguous legacy records without relabeling camera-local fits as qualified shared targets.
2. Complete the family-target versus camera-realization worker boundary. Bootstrap a provisional target from eligible evidence on any supported body when its family lacks one; fit other realizations against pinned targets. Isolate pools, histories, attempts, gates and indirect experimental influence.
3. Add reports, dependency links, current projections, invalidation hooks and fenced successor scheduling.
4. Integrate common native/control assessment and automatic scoped promotion with existing refit, uncertainty and gates.
5. Wire authenticated independent camera-bundle publication, all selectors/UI, assessment-pinned replay and evidence requests end to end.
6. Integrate qualified multi-body refitting through the common hierarchy, preserving realization/illuminant interfaces, gauge persistence and caps. Automatic cohort admission creates and assesses a new family candidate; it never overwrites the target with an independent body fit.

This assignment includes the mechanical target/realization pipeline and automatic admission/refit lifecycle. Do not claim completion from a badge-only release, the library entry point alone, or a set of independently published per-camera targets. Production activation of qualified multi-body updates requires its numerical and cross-body gates to pass. Broad experimental availability does not require every camera to pass them; unavailable evidence leaves the relevant claim experimental rather than starting a bespoke per-camera implementation project.

Release boundaries are owned by [docs/09](09-roadmap.md). The broad experimental release requires a working reference baseline and the common integrity/isolation contracts, not empirical qualification of every camera. Shared-family qualification retains P1.9 and applicable engine evidence. Do not silently declare the original full launch milestone satisfied or drop unrelated release gates.

Do not loosen fit tolerances, increase fit budgets, change support or recovery thresholds, enable full-scale studies or deploy infrastructure under this plan. The existing fitter blocker still prevents any affected numerical artifact from becoming usable. Independent metadata/report/lifecycle work can proceed without another convergence experiment.

## 11. Acceptance and verification

Permanent checks must assert caller-visible outcomes, not helper wiring or copied source text. Fit-free lifecycle tests belong in ordinary CI. Fitter-dependent worker cases use the existing integration feature/pre-release lane. Full-scale runs and `--all-features` still require an explicit maintainer request.

1. Two bodies sharing a Look family publish retrievable realization candidates against the same selected family version, without replacing each other's realization history/attempts or the canonical target. Updating a camera publishes a new independent bundle snapshot without changing the family version or old manifests. Each source sim pins its own native family, separate from the requested target. An independent diagnostic fit cannot become the family target through maximum-version selection.
2. Two epochs, and two sims mapped to the same family, preserve separate schema/control scope and attempts. New scope does not inherit qualification or erase an applicable parent gate lineage.
3. A preparation-ready camera remains contributable and discoverable without a release allowlist. A numerically valid, gate-passing realization against a publishable pinned target is explicitly usable for experimental matching without empirical qualification. Otherwise show its exact blocker and permitted diagnostics. A new family on a non-reference camera can seed one provisional canonical target without qualification or automatic root admission.
4. Default-only data qualifies no nondefault control; endpoint/interpolation support qualifies no untested interval or interaction. Qualified matching refuses an out-of-scope claim and offers the experimental path.
5. Same-RAW re-renders never cross assessment partitions. Reports expose conditional nuisance treatment without claiming independent absolute calibration.
6. Adding experimental data leaves qualified targets, gauges, support, contributor-noise/reputation/weighting state and selections unchanged, including when contributors and Look tags overlap.
7. Promotion requires applicable evidence and gates, then occurs automatically. Cohort admission schedules a qualified fit instead of copying experimental coefficients.
8. Failed, nonconverged and gate-rejected replacements preserve valid predecessors and expose diagnostics without publishing accepted artifacts.
9. Contrary new data assesses the active model; withdrawal of the final qualifying group removes its current assertion even with an empty pool.
10. Root revocation, schema/profile/rule reinterpretation and identity merges invalidate exactly the affected assertions and fence affected in-flight commits. Append-only arrivals queue successors without starving otherwise valid publication or rolling back a newer selection.
11. Reports/status changes trigger publication even without a newly published Look. All UI/download consumers preserve scope and experimental status; permalink assessment context survives later qualification while current invalidation remains visible. Body-agnostic cards remain downloadable without an unused camera CRM. Manifests/hashes remain verifiable and old pins remain historical, not silently requalified.
12. Missing evidence produces specific wanted shots; missing authoritative resources produce explicit blockers. Neither creates invented calibration, extra uploaded data, automated trust grants or routine camera-promotion moderation.

Smoke the real local worker/server/publication path with retained fixtures or a disposable database, then inspect the generated site in a browser: one canonical family target, a qualified reference realization, an experimental realization on another body, a nondefault setting outside qualification, a failed replacement and an invalidating withdrawal. Verify that changing an experimental realization does not change the pinned family bytes or the reference realization. Observe selectors, reports, rendered status, downloads and hashes. Exercise qualified admission through a compact common-path case, not a full-scale run. Reuse known numerical evidence where applicable; do not rerun reported failures merely to reconfirm them. Report any scenario blocked by the independent fitter failure instead of substituting mock results for working publication.

Run builds through `nix develop` and the main checkout's shared `target/` and `target-ui/`. One integration owner runs final checks. Do not create per-agent caches/worktrees; any exceptional isolated resource needs an owner and removal after use. Preserve UI stub databases/secrets and unrelated user work. No production deployment is authorized here.

## 12. Canonical documentation changes for implementation

The canonical contracts are integrated at the following locations. Keep them current with implementation; this table is an ownership/navigation map, not a substitute specification.

| File | Canonical ownership |
|---|---|
| [README](../README.md) | Broad experimental availability versus measured capabilities and full shared-family release |
| [docs/00](00-overview.md), [docs/01](01-architecture.md) | Shared engine, model subjects, one-way experimental influence and qualification/publication axes |
| [docs/02](02-color-pipeline.md) | Subject/realization scope, cohort eligibility, gauge persistence, evidence interpretation and unchanged numerical contracts |
| [docs/03](03-data-model.md) | Typed subjects, reports, dependency links, current selections, artifact associations and migration |
| [docs/04](04-matching-and-export.md) | Qualified versus explicit experimental matching, pinned identities, download provenance and unchanged card semantics |
| [docs/05](05-trust-and-integrity.md) | Isolation, scoped automatic eligibility, invalidation, existing human trust authority and release gates |
| [docs/07](07-contribution.md), [docs/08](08-ui.md) | Broad intake, scoped status, limitations, evidence requests and historical invalidation notices |
| [docs/09](09-roadmap.md) | Split the reference-body/experimental release from shared-family activation; attach this plan's behaviour acceptance to existing task IDs |
| [docs/10](10-open-questions.md) | Resolve qualification/publication scope while retaining Q18's empirical status and undefined absolute-calibration claims |

The upload/privacy contract in [docs/06](06-privacy-and-payload.md) is intentionally unchanged: no new pixels, RAWs, coordinates, RAW hashes or metadata fields. If implementation discovers it needs such a change, stop that part and request a separate design decision rather than expanding this plan silently.
