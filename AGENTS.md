# AGENTS.md — working conventions

For anyone (human or agent) implementing looklab. Read [README](README.md) and the doc(s) for your task first.

## Picking up work
- Tasks are defined in [docs/09-roadmap.md](docs/09-roadmap.md) by ID (e.g. `P1.5`). Reference the ID in branch names (`p1.5-single-body-fit`) and PR titles.
- A task is done when its **acceptance criteria pass in CI** (or with documented manual evidence where CI can't run it), docs are updated if behavior or thresholds changed, and the PR lists what was verified.
- If a doc is wrong or underspecified, fix it in the same PR and call it out. Don't silently diverge.
- Thresholds marked *(tentative)* may change with evidence (data + reasoning in the PR).

## Build storage and agent cleanup
- Run builds through `nix develop` from a checkout. All linked worktrees share the main checkout's `target/` for Cargo and `target-ui/` for the UI launcher. Reuse these two roots; do not create `target/agents/<name>`, per-task target roots, or per-worktree caches. Cargo lock contention is expected; wait rather than creating another cache.
- Run Cargo through `tools/build.sh --lane native -- cargo …` (or `--lane ui`) whenever more than one checkout builds. A direct `cargo` invocation in a linked worktree and one in the main checkout reuse each other's artifacts as fresh, so each can compile against the other's crates.
- Every delegated task must include this storage policy and cleanup ownership in its instructions. The integration owner is responsible for cleanup after merging or abandoning the task.
- After integration, stop the task's builds and services, remove every isolated build/cache directory the task created, and remove its clean worktree with `git worktree remove <path>`. Check for uncommitted and untracked work first; never force-remove a worktree or delete unmerged work.
- An exceptional isolated build directory must have a named owner and be removed when its command finishes, including on failure. Do not leave it until the next session.
- Do not copy build roots into a worktree. Current `nix develop` rejects unowned inherited targets and copied local target storage; name any exceptional owner with `LOOKLAB_ISOLATED_BUILD_OWNER`. In older topic checkouts use the current main checkout's shell explicitly: `nix develop /absolute/main/checkout#default -c COMMAND`.
- Never delete shared build roots while another task uses them. UI target roots can contain `stub-state/`, including a database and server secret; preserve that state before retiring a UI root.

## Rust conventions
- **Stable Rust**, edition 2021 (or 2024 once adopted). `cargo fmt`; `cargo clippy --all-targets -- -D warnings`.
- SPDX header in every source file: `// SPDX-License-Identifier: AGPL-3.0-or-later`.
- Core crates (everything except `server`, `worker`, `cli`, `web`) must build for `wasm32-unknown-unknown`: no threads, filesystem or `std::time::Instant` in core paths. Platform specifics go behind traits or features. The browser uses several **single-threaded** workers; don't design for shared-memory WASM threads.
- **Numerics and determinism:**
  - f64 for fitting and color math; f32 only in sample storage.
  - **All transcendental math in core crates uses the `libm` crate.** `clippy.toml` bans **every** transcendental method on `f64` and `f32` via `disallowed_methods`: `powf, powi, exp, exp2, exp_m1, ln, log, log2, log10, ln_1p, sin, cos, tan, sin_cos, asin, acos, atan, atan2, sinh, cosh, tanh, asinh, acosh, atanh, cbrt, hypot`. CI also fails on any `f32`/`f64` method outside an allow-list (`sqrt`, `abs`, `min`, `max`, `clamp`, `floor`, `ceil`, `round`, `trunc`, `mul_add`, `signum`, `copysign`, `to_bits`, `from_bits`, `to_le_bytes`, `from_le_bytes`, `total_cmp`, `is_*`), so new std methods can't slip in; `cargo xtask float-methods` re-derives the stable method set from the toolchain's rust-src and fails when `clippy.toml` drifts. Use `libm::log1p`/`libm::expm1` for CAM16-UCS's ln(1 + c₂M) and its inverse.
  - The Look evaluation path uses scalar code or fixed-order reductions only: no `faer`/`pulp` or other runtime-dispatched SIMD, no fast-math. Native and WASM outputs must be **bit-identical** (tested).
  - **f64-bearing JSON must parse bit-exactly.** `serde_json`'s `float_roundtrip` feature is pinned in `[workspace.dependencies]`; enabling it per crate makes the parsed bits depend on the cargo package selection. `cargo xtask bitid` compares each workload against both the minimal and the workspace-unified native selection.
- `unsafe` only with a justifying comment and a test. Prefer none.
- Errors: `thiserror` in libraries, `anyhow` in binaries.
- Every randomized algorithm takes an explicit seed parameter; tests use fixed seeds. **Exception (privacy):** in production, the sampling/shuffle seed is fresh CSPRNG output, never derived from server-known values, never uploaded or stored, and treated as sensitive in UI and logs ([docs/03 §2.1](docs/03-data-model.md)).
- **Contribution upload content comes from the `looklab-wasm` module**, including the canonical upload envelope ([docs/03 §2.3](docs/03-data-model.md)). JavaScript only moves those bytes between the module, WebAuthn and `fetch`; it never builds, edits or adds contribution content. The separate pre-boot diagnostic channel accepts only the failure-location whitelist in [docs/06 §1.1](docs/06-privacy-and-payload.md#11-automatic-error-diagnostics), never console arguments, exception messages or contribution data.
- **Payload reproducibility:** the reference prep is the `looklab-wasm` prep module. It must not use WASM relaxed-SIMD (CI checks the module's target features and validates the binary without it), and no NaN may reach stored values. Don't make prep output depend on anything except inputs, the seed file and the prior-transform version (no timestamps, no host-dependent paths). `looklab-cli verify-payload` must reproduce browser payloads byte for byte under wasmtime.
- Cite each formula's source in doc comments (paper + equation, or the colour-science function).

## Color-science porting
- Port from colour-science (BSD-3) with attribution in `NOTICE` and in module docs.
- Every ported function gets golden vectors from `tools/golden/` (pinned colour-science version). CAM16 vectors for the reference conditions use `discount_illuminant=True`. Regenerate only via the scripts.
- Don't copy code from dtsolve (no license) or darktable-lut-generator (license unverified). Implementations are original.

## Security and trust invariants
- Trust is designed for **hobby scale** ([docs/05](docs/05-trust-and-integrity.md)): bound what can't be verified, and make rollback cheap.
- Contribution kinds are client-claimed. Never let a client claim, reputation or elapsed time move a credential out of the `new` tier or past the new-tier group cap. Only human decisions (an admin grant or a seeder endorsement) change tier.
- Serial merging must never increase the weight of any image belonging to the merged credentials, and must never transfer tier or reputation between credentials. Other contributors' renormalized shares may rise.
- Grants, endorsements, root admissions, moderation actions, re-anchors and reverts always write an audit-log row. The audit log is append-only: no UPDATE/DELETE in application code.
- Every published artifact is listed by hash in its version manifest. Clients check hashes.
- Changes to `fit` or `trust` must pass targeted recovery and trust-policy tests. Fitter-to-trust integration follows the validation tiers below.

## Privacy invariants
- Nothing derived from pixel positions may cross the `looklab-prep` → `looklab-samples` boundary. The sample type has no coordinate fields; order is shuffled with a fresh, never-uploaded CSPRNG seed. Re-render groups share one sample set; never send a RAW or content hash.
- The sanitizer is whitelist-based. A new metadata field needs an update to [docs/06](docs/06-privacy-and-payload.md).
- Never store IP addresses. Never publish credential public keys or serial HMACs; logs and changelogs use internal ids or display handles.
- Raw serial numbers exist only in a type that implements neither `Serialize` nor `Debug`, and are HMAC'd on arrival. A test scans the DB and logs for raw serials.

## Data and fixtures
- RAW samples with a stable public CC0 host are fetch-only: `fixtures/manifest.toml` records file, sha256, size, source and consent, and `tools/fetch-fixtures.sh` downloads them into the gitignored `fixtures/cache/`.
- A donated RAW with no other host goes in Git LFS under `fixtures/donated/` with `committed = "lfs"` and explicit recorded consent in the manifest; an approved in-app fixture donation writes both.
- Test vectors under 1 MB that a test needs as a compile-time input (fuzz seeds, card conformance `.llk`, golden JSON, DNG crops, pattern PNGs) stay in plain git.
- Prefer synthetic tests (injected look deltas, simulated bodies, simulated attackers) for correctness. Use real fixtures for realism.

## Tests that must exist
- Unit, golden (color) and property tests (round-trips, sanitizer, monotone tone, identity Look, sample-reader limits).
- `fit`/`trust`: targeted synthetic recovery and policy tests in required CI, plus compact real fitter-to-trust integration.
- Native vs wasm32 bit-identity tests for `color` and Look evaluation.
- `criterion` benchmarks for Look evaluation, the optimizer and prep; retain the 20% regression threshold when comparing benchmark runs.
- Compact fitter-to-trust integration is a pre-release gate, not part of ordinary CI. Its measured CI job exceeded the approximately two-minute budget; record job wall time and test execution separately when reconsidering placement.
- Worker cases that execute normal-budget native or consistency fits require `looklab-worker/integration` and run before releases, including fits performed by fixture construction. Add every new integration test target to the pre-release workflow's selection. Fit-free worker checks and budget-short-circuit moderation checks stay in ordinary CI. The 1,000-image benchmark and ignored fixture-convergence diagnostic require `looklab-worker/full-scale`.
- `looklab-fit` recovery cases that need more than 120 s when run alone at `CARGO_PROFILE_TEST_OPT_LEVEL=3` require `looklab-fit/long-tests`; the end-to-end `looklab-cli` fit cases behind `looklab-cli/long-tests` follow the same rule. Ordinary CI runs only tests that finish within 120 s solo. Both selections run before releases through **Pre-release integration**; add every new long case to the pre-release workflow's selection.
- Full-scale numerical studies, growing-pool simulations and benchmark runs require an explicit maintainer request. They do not run on pushes, pull requests, schedules or releases by default.
- Agents may recommend a full-scale run and name the affected behavior. They must not dispatch the workflow, enable `full-scale`, or run tests with `--all-features` without that request. Compiling or listing the opt-in test inventory does not execute it.
- Keep targeted tests discriminating: preserve valid honest controls and reject the intended bad outcomes. Smaller fixtures must not turn a policy test into success through unsupported fits or blanket rejection.

## Data files (`data/`)
- YAML, validated by `looklab-schema` (`looklab-cli validate data/`). The same validator runs in the browser.
- Bodies and interfaces in `data/` are **snapshots exported from the server authority**. Change them through the in-app proposal flow, not by hand-editing, except for bootstrap data before the server exists.
- Makernote mappings need a fixture; without one they're marked `provisional`.
