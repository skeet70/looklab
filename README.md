# looklab

A tool, and eventually a web app, for **measuring camera "looks" (film simulations, picture profiles, LUTs) rigorously, and telling you how to reproduce a target look on your own camera**.

- Build a **target look** from RAW+JPEG pairs attested to use a given sim. Data can come from one camera or be crowdsourced from many, so the look is separated from any single sensor.
- **Compare** any two looks in a perceptual appearance space (CAM16-UCS), broken down into tone, neutral tint, chroma, hue and lightness components, with uncertainty.
- **Match** a look on another body. You get either the best in-camera settings for stock firmware, or an exact LUT/curve bundle for firmware that can load one.

## Local site

From the repository root, run `site/scripts/dev.sh`. It enters `nix develop`, generates `site/data` if missing, builds the development stub once, then serves the UI with automatic reload at <http://localhost:8081/>. The stub listens on 8098; Trunk proxies `/api/*`, `/status/*`, `/log.json`, `/lists/*`, `/bounties.json` and `/manifests/*` to it. Open status pages request HTML; API fetches receive JSON. The stub uses the public 8081 origin for passkeys. Port 8080 and `site/dist` are not used. SIGTERM stops startup builds and both server processes; no TTY is required.

Artifacts, the served distribution and persistent stub state live in `target-ui/`, separate from other Cargo builds. Set `LOOKLAB_UI_TARGET_DIR` to choose another dedicated directory. Trunk uses the opt-in `web-dev` Cargo profile: no debug information, incremental compilation, 256 codegen units and dependency optimization at level 1. Trunk's Cargo builds run offline after the native stub build resolves dependencies. The launcher sets `LOOKLAB_WORKER_WASM=reuse`: existing `site/generated/wasm/*.wasm` are reused, or built in release once when missing. Set `LOOKLAB_WORKER_WASM=build site/scripts/dev.sh` after changing worker code. Ordinary Trunk, release, CI and Pages builds retain the release worker build and validation.

Expected warm rebuilds are about 31 s for a page `.rs` edit and 15 s for `site/styles.css`, measured in the isolated preview checkout with the same build structure. Before: cold debug 475 s, page edit 128 s, CSS edit 107 s. Worker reuse plus disabled debug information gave a 175 s cold build; dependency optimization reduced the wasm output from 77.5 MB to 56.4 MB. The live checkout's first `web-dev` build took 343 s under concurrent build load. Other sessions changing shared crates or holding Cargo's registry lock can delay rebuilds. Missing worker assets add a one-time release build.

Run `site/scripts/dev.sh --regen-data` after scene-data schema changes to regenerate synthetic Looks, bodies, scenes, public camera match bundles and audit entries before serving. `site_data` includes the public match artifacts before computing release manifest hashes. The separate `publish_match_fixture` step adds only the hidden `synthetic-match` end-to-end fixture and remains required for synthetic match and verification checks. To regenerate without launching the site, run both commands:

```sh
CARGO_TARGET_DIR="$PWD/target-ui-data" nix develop -c cargo run -p looklab-publish --example site_data -j 6 -- site
CARGO_TARGET_DIR="$PWD/target-ui-data" nix develop -c cargo run -p looklab-wasm --features fixtures --example publish_match_fixture -j 6 -- site
```

Both examples accept a destination directory in place of `site`, including a directory under `/tmp`; `publish_match_fixture` defaults to the workspace's `site` directory when omitted. Outside the launcher, `LOOKLAB_API_BASE` names the API origin at build time (unset: the page's own origin), and `LOOKLAB_PREVIEW=1` builds a server-less preview with uploads, status, proposals and moderation disabled. The launcher unsets both to use its local stub. The `Pages` workflow publishes that preview when run manually.

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
