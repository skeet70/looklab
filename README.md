# looklab

A tool, and eventually a web app, for **measuring camera "looks" (film simulations, picture profiles, LUTs) rigorously, and telling you how to reproduce a target look on your own camera**.

- Build a **target look** from RAW+JPEG pairs attested to use a given sim. Data can come from one camera or be crowdsourced from many, so the look is separated from any single sensor.
- **Compare** any two looks in a perceptual appearance space (CAM16-UCS), broken down into tone, neutral tint, chroma, hue and lightness components, with uncertainty.
- **Match** a look on another body. You get either the best in-camera settings for stock firmware, or an exact LUT/curve bundle for firmware that can load one.

## Build storage

Run builds through `nix develop` from a checkout. The development shell defaults to two shared build roots in the main checkout: `target/` for Cargo commands and `target-ui/` for the UI launcher. Linked worktrees reuse those same directories.

Cargo serializes competing builds with its existing locks. Wait for those locks rather than creating per-agent or per-task build directories. Unset old `CARGO_TARGET_DIR` and `LOOKLAB_UI_TARGET_DIR` overrides before re-entering the development shell.

The integration owner runs the workspace-wide gates once after combining changes. Subagents run targeted crate or test checks for their slice and hand off their results. Keep one build owner per shared root; redundant Cargo commands queue behind the same locks. Use the checked-in profiles rather than per-agent profile overrides.

After merging or abandoning an agent task, stop its builds and services, remove any isolated build caches it created, and remove its clean worktree with `git worktree remove <path>`. Preserve uncommitted, untracked and unmerged work. The integration owner is responsible for this cleanup.

UI build directories can contain `stub-state/`, including a local database and server secret. Preserve that state before retiring a UI build directory, and never delete a build directory while another task is using it.

## Build pipelines

CI has independent `lint`, `native` and `wasm release` lanes plus twelve release trust test shards. The required `ci` check succeeds only when every lane and shard succeeds. Trust uses nextest count partitions with no fail-fast or default filter; shard 1 also runs release doctests. Native tests explicitly enable `looklab-fit/synth` in the workspace run; fit acceptance is not run a second time. Prep and scene WASM are built and validated before tests, which consume those artifacts without invoking Cargo.

Pages generates release site data and release worker WASM in parallel. The UI job downloads both products and their receipts from the same run, verifies them, then builds the release preview. Worker optimization, UI `-Oz`, release LTO and the `/looklab/` public URL are unchanged.

Cargo caches are partitioned by build family, toolchain, lock/configuration and compiler flags; incremental directories and local stub state are excluded. Cache saves are restricted to `main`. CI saves compiler artifacts after failed gates unless cancelled; generated products are saved only after their producer succeeds. One trust shard saves the shared release compiler cache. Generated-product caches are hints: source/tool/profile identities, output hashes and artifact policy are checked before reuse.

Nix uses public binary substituters rather than a per-store GitHub Actions cache proxy. Synthetic trust shards skip RAW fixture downloads.

`tools/build_products.py` owns worker and site-data freshness. Receipts and shared writer locks live under the main checkout's `target/build-products/`. Worker inputs include the local Cargo source closure and the explicit compile-time input trees; data also binds the fetched scene sources. CSS and UI-only source edits do not invalidate these products. New include targets or generator modules outside the supported input selection fail closed; extend the selection in that helper when adding them. Run `nix develop -c python3 -m unittest discover -s tools/tests -v` for the isolated cache-boundary tests.

Single-run validation on 2026-11-06 used application commit `c10b5d2` plus build and fitting-performance changes, excluding concurrent uncommitted application edits. Pages times cover the build critical path, not deployment:

| Pages run | Elapsed | Evidence |
|---|---:|---|
| Original pipeline | 22m 42s | [Baseline](https://github.com/skeet70/looklab/actions/runs/42109884751) |
| New pipeline, first Cargo/product cache population | 16m 34s | [Cold run](https://github.com/skeet70/looklab/actions/runs/42180363234) |
| New pipeline, warm caches before host-cache correction | 8m 13s | [Warm run](https://github.com/skeet70/looklab/actions/runs/42182424417) |
| New pipeline, warm host and WASM UI cache | 7m 32s | [Exact-source rerun](https://github.com/skeet70/looklab/actions/runs/42192708299/attempts/2) |
| Main, unchanged Rust source | 6m 48s | [Successful deployment](https://github.com/skeet70/looklab/actions/runs/42262170856) |
| Main, changed fitting Rust source | 12m 25s | [Successful deployment](https://github.com/skeet70/looklab/actions/runs/42271818181), not an unchanged-source warm run |
| Main, guarded Hessian and input/scoring reuse | 12m 28s | [Successful deployment](https://github.com/skeet70/looklab/actions/runs/42286293145), changed Rust source |

The corrected warm UI run restored its populated host/WASM cache and recompiled only workspace crates; no third-party dependencies recompiled.

Local pipeline measurements used the same application base and retained the existing shared dependency cache. This was not an empty-cache build or an old/new pipeline comparison. Both passes included source capture, data handling, stub compilation, workers, Trunk and CSS publication.

| Local pass | Elapsed |
|---|---:|
| First pass, generated products missing | 7m 02s |
| Unchanged pass, verified products reused | 7.10s |

Full CI measurements used the same application base and failed Clippy, the CLI E2E flow and 10 release trust red-team cases:

| CI configuration | Elapsed | Evidence |
|---|---:|---|
| Three lanes, unsharded release trust suite | 52m 41s | [Pre-shard run](https://github.com/skeet70/looklab/actions/runs/42180330465) |
| Four trust shards, first trust compiler-cache population | 40m 08s | [Sharded run](https://github.com/skeet70/looklab/actions/runs/42190766989/attempts/1) |
| Four trust shards, cache-populated rerun with HTTP 429 restore failures | 38m 26s | [Same-source attempt 2](https://github.com/skeet70/looklab/actions/runs/42190766989/attempts/2), not a fully warm comparison |
| Four trust shards with exact-order covariance/pass reuse | 25m 26s | [Pushed-main run](https://github.com/skeet70/looklab/actions/runs/42271818190), same 124 passed and 10 failed |
| Four trust shards with guarded Hessian and input/scoring reuse | 27m 05s | [Pushed-main run](https://github.com/skeet70/looklab/actions/runs/42286293272), same 124 passed and 10 failed |

The sharded run executed all 134 release trust cases: 124 passed and 10 failed. Its required aggregate check stayed red. The CLI E2E failure also reproduced on unmodified `c10b5d2` with the original Wasmtime defaults. No successful same-source full-CI baseline is available; the failing native E2E binary prevents later workspace tests from running.

The partition inventory at `ed383e9` selected all 134 trust cases exactly once with twelve count partitions. Four and eight partitions co-located the two longest cases; twelve put them in separate jobs. Fit settings, test filters and gates are unchanged. The twelve-shard full-CI critical path has not yet been measured.

An isolated native-only [attempt 3](https://github.com/skeet70/looklab/actions/runs/42190766989/attempts/3) restored its compiler cache, compiled in 4m 44s, then hit the same CLI failure; the job took 13m 02s. After the timed runs, removing the per-store Nix cache proxy passed a [seven-runner setup/cache smoke](https://github.com/skeet70/looklab/actions/runs/42203798464): all seven compiler caches restored and all seven project toolchains ran, with no HTTP 429 observed. That smoke did not rerun the test suites or measure full-CI latency.

### Trust fitting performance

Cholesky updates and triangular solves interleave independent accumulators without changing their scalar reduction order. Hessian accumulation uses one triangle only when the initial matrix is bitwise symmetric; asymmetric matrices retain independent updates. The completed triangle is mirrored after the pass.

Image contexts store `2^exposure` once, and prediction-only evaluation constructs one `LookInput` per sample. Model-error held-out scoring computes predictions without unused Jacobians. Fit options, sample counts, seeds, stopping criteria and acceptance thresholds are unchanged.

The complete thin-root scenario ran all ten releases on the same workstation:

| Application/performance revision | Elapsed, compilation excluded | Peak RSS |
|---|---:|---:|
| `c10b5d2` | 22m 31s | 424 MiB |
| `35f282a` | 17m 26s | 428 MiB |
| `3a9c6c8` | 17m 41s | 395 MiB |
| Guarded Hessian and input/scoring reuse | 15m 01s | 465 MiB |

Every run retained the same 2/10 publication outcome and first over-ceiling shift `2.102994928935835`. These are separate runs with concurrent workstation activity; scheduling, clock frequency and allocator residency were not controlled. They do not establish green-CI latency.

Three alternating baseline/candidate pairs also exercised converged public fits with default options on 3 images × 1,600 samples and 20 images × 200 samples, using seeds 14 and 71. All six executions returned byte-identical full `FitResult` debug outputs, including probe uncertainties. The new mathematical boundary tests cover inverse batch tails and prefilled asymmetric Hessians.

## Local site

From the repository root, run `site/scripts/dev.sh`. It enters `nix develop`, captures source files, builds the data, stub, workers and UI, then serves the UI at <http://localhost:8081/>. A TCP proxy forwards that public origin to the stub on 8098. Passkeys use the public 8081 origin. The launcher exclusively locks its UI target root. SIGTERM, SIGINT and SIGHUP stop builds, the stub, proxy and stylesheet watcher; no TTY is required.

The captured source view, served distribution and persistent `stub-state/` live in `target-ui/`. Set `LOOKLAB_UI_TARGET_DIR` to choose another shared UI root. The stable `target-ui/source/` view uses copied or reflinked files, not hardlinks to active sources; compiler artifacts still share `target-ui/`. Native captured builds similarly use `target/source/` and the shared `target/` cache. Lane locks serialize capture and builds without locking writers in the checkout. Trunk uses the `web-dev` profile: no debug information, incremental compilation, 256 codegen units and dependency optimization at level 1. Its Cargo build runs offline after the native stub resolves dependencies. Missing or stale workers are rebuilt in release and validated. Use `nix develop -c sh site/scripts/wasm-assets.sh --force --scene` to force only the scene worker, or omit the selector for all workers.

Unchanged source bytes retain their view mtimes. Changed inputs receive fresh mtimes even when checkout timestamps were preserved; an older capture policy is refreshed once on cutover.

Captured builds use a source-view-specific `RUSTC_WORKSPACE_WRAPPER` launcher in the shared target root. Workspace artifacts cannot be reused from raw Cargo builds or another captured view; registry dependency artifacts remain shared. An existing `RUSTC_WORKSPACE_WRAPPER` environment setting is chained through the launcher. Configure custom workspace wrappers through that environment setting for captured builds; the runner reserves Cargo's workspace-wrapper slot.

Stylesheet edits publish directly to the served distribution and repaint open pages through a development-only stamp poller, without Cargo, wasm-bindgen or wasm-opt. Other source edits queue the latest generation: the current captured build finishes rather than restarting when another agent writes a file. The last successful distribution and stub remain available during builds; failed builds wait for the next edit instead of retrying unchanged inputs. Successful publication exchanges the distribution and installs the matching stub/body metadata, with rollback on failure. Tests, benches and Markdown do not queue UI builds. Restart after changing launcher/helper logic. `site/scripts/dev.sh --once` builds and serves exactly one captured generation while retaining live CSS updates.

Release photo previews load the verified thumbnail before preparing the full scene. Full-size RGB16 PNG decoding and interpolation-sample construction run in an import-free scene worker using the same scalar Rust code as the reference path. The main thread validates the transferred prepared samples, renders the LUT and paints the canvas. Home, Look and Compare keep the current viewer while upgrading; a pinned match displays the target look while the settings worker runs. Browse requests omit matching covariance/model files, and prepared scenes are reused across routes. The release profile retains optimization level 3, LTO and one codegen unit; Trunk applies `wasm-opt -Oz` to the UI. Prep and match worker build settings are unchanged.

To refresh the release preview served from `/tmp/looklab-release-dist` on port 8082, run `nix develop -c tools/build.sh --lane ui -- sh -c 'sh site/scripts/site-data.sh --release && sh site/scripts/wasm-assets.sh && cd site && LOOKLAB_PREVIEW=1 CARGO_NET_OFFLINE=true trunk build --locked --release --dist /tmp/looklab-release-dist'` from the repository root. Data, workers and UI then use one captured generation. The existing preview service picks up that directory without a restart.

Measure empty-cache Chromium navigation to the first photo and full-size upgrade from the repository root:

```sh
nix develop -c env \
  PLAYWRIGHT_MODULE="$(nix eval --raw nixpkgs#playwright-driver)/index.mjs" \
  PLAYWRIGHT_BROWSERS_PATH="$(nix eval --raw nixpkgs#playwright-driver.browsers)" \
  node site/scripts/first-photo.mjs
```

The benchmark writes timestamps, resource transfers, stage timings and screenshots to `/tmp/looklab-first-photo`. `LOOKLAB_PERF_ORIGIN`, `LOOKLAB_PERF_DIST` and `LOOKLAB_PERF_OUT` override the origin, distribution and output directory. Set `LOOKLAB_PERF_TRACE=1` for separate diagnostic Chromium traces with WASM compilation, instantiation and rendering events; use an untraced run for the timing comparison. The targets are a first photo below 2 s on localhost and LUT application below 200 ms. The first photo on a pending match is its verified target, not a completed settings prediction. Leptos supports lazy routes, but pinned Trunk 0.21.14 has no split-before-bindgen integration; the UI remains one WASM bundle. [Trunk #1022](https://github.com/trunk-rs/trunk/issues/1022) tracks upstream support.

The photo benchmark also reports main-thread long tasks between first-photo and full-size-scene paint. With the same Playwright environment, `node site/scripts/scene-worker-check.mjs` compares the prepared sample bytes and rendered RGBA bytes against the UI-compiled main-thread Rust reference on all nine published scenes. It verifies the index, scene PNGs and LUT hashes and writes `/tmp/looklab-scene-worker-check/parity.json`.

For a local workspace run, use `nix develop -c tools/build.sh`. It captures one source generation, prepares prep/scene WASM, then runs `cargo test --locked --workspace --features looklab-fit/synth`. A targeted command runs exactly as supplied, for example `nix develop -c tools/build.sh -- cargo test --locked -p looklab-fit --features synth --test sample_trust`. Use `--lane ui -- <command>` when a captured command needs the shared UI build root. The prep, payload-verification and scene ABI tests require fresh worker artifacts and only verify them; raw Cargo callers must first run `nix develop -c sh site/scripts/wasm-assets.sh --prep --scene` against the same source view. `nix develop -c cargo xtask prep-module` uses that producer. To check all workers without rebuilding, run `nix develop -c sh site/scripts/wasm-assets.sh --check`.

Each captured UI generation checks site-data freshness; `site/scripts/dev.sh --regen-data` forces its first data pass. `site/scripts/site-data.sh` runs `site_data` and then `publish_match_fixture`, including the hidden synthetic-match end-to-end fixture. The default uses the optimized test profile; `--release` preserves the production generator profile. Each profile has a distinct receipt. To generate or check data without launching the site:

```sh
nix develop -c sh site/scripts/site-data.sh
nix develop -c sh site/scripts/site-data.sh --check
nix develop -c sh site/scripts/site-data.sh --release
```

The data wrapper accepts a destination directory in place of `site`, including a directory under `/tmp`. Generation stages on the destination filesystem and atomically exchanges the current data tree before sealing its receipt. Outside the launcher, `LOOKLAB_API_BASE` names the API origin at build time (unset: the page's own origin), and `LOOKLAB_PREVIEW=1` builds a server-less preview with uploads, status, proposals and moderation disabled. The launcher unsets both to use its local stub. The `Pages` workflow deploys only from `main`; manual runs on other branches build without deployment.

New stub profiles have no roles. For local moderation checks, POST `/api/__stub/moderator/{credential}` with the base64url credential ID. POST `/api/__stub/access/{credential}` with the JSON string `"moderator"`, `"admin"` or `"seeder"` to exercise the other review roles. Reload to refresh the role menu from the server. These grant endpoints exist only in the development stub; other `/api/*` routes reach the real server handlers. `site/checks/e2e.mjs --flow account` covers profile creation and work-based activation, local sign-out, sign-in again, independent browser sessions, global revocation and role refresh.

## Design docs

Read in order. Each doc is self-contained enough to hand to an implementer.

| # | Doc | What it covers |
|---|-----|----------------|
| 00 | [Overview](docs/00-overview.md) | Goals, non-goals, glossary, core principles |
| 01 | [Architecture](docs/01-architecture.md) | Crates, client/server split, tech choices, licensing |
| 02 | [Color pipeline](docs/02-color-pipeline.md) | Input transforms, the Look model, fitting, uncertainty, probes, diagnostics |
| 03 | [Data model](docs/03-data-model.md) | Entities, firmware epochs, file formats, DB schema |
| 04 | [Matching & export](docs/04-matching-and-export.md) | Control schemas, control-response models, optimizer, firmware card format, software interface profiles, LUT baking |
| 05 | [Trust & integrity](docs/05-trust-and-integrity.md) | Tiers, reputation, weighting, caps, release gates, rollback (hobby scale) |
| 06 | [Privacy & payload](docs/06-privacy-and-payload.md) | What leaves the browser, metadata sanitization, payload inspector |
| 07 | [Contribution](docs/07-contribution.md) | The contribution ladder, capture protocols, detection |
| 08 | [UI](docs/08-ui.md) | Audiences, principles and vocabulary, pages, flows, visual system |
| 09 | [Roadmap](docs/09-roadmap.md) | Phases, tasks, acceptance criteria |
| 10 | [Open questions](docs/10-open-questions.md) | Decisions not yet made |
| — | [References](docs/references.md) | Prior art and papers |

Working conventions for implementers (human or agent): [AGENTS.md](AGENTS.md).

## License

Code: AGPL-3.0-or-later. Data and exports: see [01-architecture § Licensing](docs/01-architecture.md#licensing).
