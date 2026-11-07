# Deployment

The production process model is two native binaries over one local state directory: `looklab-server` owns the HTTP listener, SQLite, private blobs and the published data directory; `looklab-worker` owns fits, trust jobs and static publication. Both open the same SQLite file. There is no queue broker, object store or external cache. (`docs/ops/litestream.md` covers database replication.)

## Modes

- **Single origin (recommended).** The server serves `/api/*`, `/healthz`, `/readyz`, `/data/*` and the SPA from `site.dist_dir` and `site.data_dir`. Set `origin`/`rp_id` to the public site hostname and leave `cors_origins` empty. Passkeys, fetches and the publication-root endpoint are all same-origin.
- **Split origin.** The SPA lives on a static host or CDN; the API serves the live catalog and publication artifacts on another hostname. Build the SPA with `LOOKLAB_API_BASE=https://api.example`, configure the API's `site.data_dir`, and list the page origin in `cors_origins`. Set `origin` and `rp_id` to the public page hostname, not the API hostname. No cookies are issued; contributor and moderator sessions are `Authorization: Bearer` tokens, so CORS never needs credentials or a wildcard.
- **API only.** Omit `site.dist_dir` and `site.data_dir`. Static pages and `/data` must come from elsewhere, and the SPA needs a compiled site-root pin or a split-origin root endpoint.
- **Distribution with baked data.** Set `site.dist_dir` without `site.data_dir`; when the distribution contains its own `data/index.json`, the server advertises that baked root and serves it through the SPA fallback. This suits a backend that also ships a snapshot, not a live-updating catalog.

The browser trusts the configured API origin for the publication root: it fetches `/data/site-root.json` and rejects any `index.json` whose SHA-256 differs. The serverless preview keeps the digest compiled into the wasm. Artifacts remain verified against their manifests and manifests against the index, so a changing worker catalog is browsable without rebuilding the SPA or weakening hash checks.

The GitHub Pages workflow stays a serverless preview by default. Set the repository variable `LOOKLAB_PUBLIC_API_BASE` to the public API origin (for example `https://api.looklab.example`) to build the split-origin page instead: the workflow then drops `LOOKLAB_PREVIEW`, compiles `LOOKLAB_API_BASE`, and the deployed page reads the live root and catalog through that API. Keep the API's `origin` and `rp_id` on the Pages hostname and add the Pages origin to `cors_origins`.

## State and configuration

| Path | Contents | Access |
|---|---|---|
| `/var/lib/looklab/looklab.sqlite3` (+ `-wal`, `-shm`) | credentials, grants, contributions, jobs, audit log | private, never served |
| `/var/lib/looklab/blobs` | uploaded sample blobs and artifact bytes | private, never served |
| `/var/lib/looklab/community` | community snapshot staging | private, never served |
| `/var/lib/looklab/server-secret` | 32 random HMAC bytes, mode 0600 | private, never served |
| `/var/lib/looklab/data` | shipped `data/` tree: bodies, transforms, interfaces, probes, card conformance vectors | read-only to the worker |
| `/var/lib/looklab/data/scenes` | prepared CC0 photo scene PNGs and their provenance index | read-only to the worker |
| `/var/lib/looklab/site` | worker publication root (`--site`) | worker writes, not served |
| `/var/lib/looklab/site/data` | published data tree | public under `/data/` |
| `/var/lib/looklab/site/site-root.txt` | index digest for static builds | not served |
| `/var/lib/looklab/site/dist` | built site distribution from `site/scripts/build-site.sh` | public at `/` |

Both units set `StateDirectoryMode=0700` and `UMask=0077`; `deploy/run-local.sh` sets `umask 077` before creating its state directory, so new database, blob and secret files are owner-only. Do not chmod an existing project or user directory to enforce this; create a new state root instead.

Every worker publication writes `data/developers/conformance-v1.zip` and its SHA-256 metadata alongside the catalog, from the `data/card/conformance-v1` tree, so the live developer download works without the sample-site generator. A publication fails rather than omitting the download when that source tree is absent.

Photographic preview scenes are prepared offline; the worker never fetches sources or decodes RAW. After `tools/fetch-fixtures.sh`, generate the directory from the CC0 fixtures:

```sh
nix develop -c cargo run --locked -p looklab-publish --features photo-scenes --example prepare_scenes -- \
  --manifest fixtures/manifest.toml --cache fixtures/cache --out /var/lib/looklab/data/scenes
```

It writes one 16-bit PNG per scene plus `index.json`, which records each scene's kind, title, source URL, license, source SHA-256 and PNG SHA-256. The worker verifies every PNG against that index and publishes the scenes with their provenance; a digest or size mismatch fails the publication. Without the directory it publishes the three test charts and logs a warning.

Camera processing also requires `data/transforms/<make>-<model>.yaml`. The shipped X-S10 and X-T4 files contain DNG ColorMatrix endpoints extracted from the licensed or authorized local sources; they are input priors, not native Look measurements or root admissions. Body/schema YAML alone does not supply that calibration.

For another camera, `looklab prep --raw <RAW> --jpeg <JPEG> --out <private-directory>` emits a `transform.yaml` from the RAW's DNG profile. Review its body and version, then install that file under the worker's transform directory with the body's filename. Keep the photographs, samples and sensitive seed file private. Scoring fails when a required camera prior is absent; it does not substitute an identity matrix.

The server creates the secret with OS randomness when the file is absent and keeps it stable across restarts. Changing it invalidates serial HMACs and every outstanding signed upload, so back it up with the database. Do not place the database, blob store, secret, `data/bodies` source tree or donation fixtures under `site.dist_dir`; only the worker output belongs at `/data/`.

Configuration comes from `/etc/looklab/looklab.toml` (see `deploy/looklab.toml.example`) or the `LOOKLAB_*` environment variables. The serving controls are `site.dist_dir` / `LOOKLAB_SITE_DIST`, `site.data_dir` / `LOOKLAB_SITE_DATA`, and `cors_origins` / comma-separated `LOOKLAB_CORS_ORIGINS`. `origin` must be the exact public page origin and `rp_id` its host or a parent domain; HTTP origins are accepted only for `localhost` and `127.0.0.1`. Nonce responses advertise the configured RP ID, and the SPA uses that value for assertions as well as the server's registration options.

## First start

Create a `looklab` system user/group and install both service files under `/etc/systemd/system`. Build the binaries with `nix develop -c tools/build.sh --lane native -- cargo build --locked --release -p looklab-server -p looklab-worker`, then install them at `/usr/local/bin`. Build the SPA through the UI source-generation lease with `LOOKLAB_PREVIEW` and `LOOKLAB_API_BASE` unset and an absolute `--dist` path: `nix develop -c tools/build.sh --lane ui -- sh site/scripts/build-site.sh --release --dist /var/lib/looklab/site/dist`. Copy `data/` to `/var/lib/looklab/data`, generate the photo scene directory described above into `/var/lib/looklab/data/scenes`, and copy `deploy/looklab.toml.example` to `/etc/looklab/looklab.toml`, then edit the origin, RP ID and paths. Initialize and start:

```sh
install -o looklab -g looklab -d /var/lib/looklab /var/lib/looklab/site
sudo -u looklab /usr/local/bin/looklab-server init --config /etc/looklab/looklab.toml
sudo systemctl daemon-reload
sudo systemctl enable --now looklab-server looklab-worker
```

`init` creates the missing state directories, generates the server secret once and runs every SQLite migration. A fresh database has no published catalog; the worker's startup publishes one from `data/` even when there are no Looks yet, so the SPA can render an explicit empty state. `/readyz` returns 503 until that first `data/index.json` exists.

For a local production-shaped run without systemd:

```sh
deploy/run-local.sh
```

It stores state under `${XDG_STATE_HOME:-$HOME/.local/state}/looklab`, incrementally builds development binaries and the SPA from captured source generations, serves <http://localhost:8080>, and starts the worker before the server. Override the port with `LOOKLAB_PORT` and the state root with `LOOKLAB_STATE_DIR`. Set `LOOKLAB_DATA_DIR` to launch from another `data/` tree, for example one with prepared scenes under `scenes/`; that tree also supplies the bodies and transforms. Installed production services use the release binaries.

## First admin

Registration creates a credential on first use; it has no roles. Grant the first admin offline through the audited CLI actor introduced in `looklab-cli`:

1. Open the site, create a profile with a display handle you control, and complete the passkey prompt.
2. Find the internal credential id when you do not want to match by handle: `sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' "SELECT id, display_handle FROM credentials WHERE status='active' ORDER BY created_at DESC LIMIT 5"`.
3. Run exactly one selector:

```sh
sudo -u looklab /usr/local/bin/looklab-server bootstrap-admin \
  --config /etc/looklab/looklab.toml \
  --credential <internal-id> \
  --reason "first admin bootstrap"
```

`--display-handle <name>` is accepted when exactly one active credential carries it. The command writes the grant and its audit entry in one transaction with actor `cli`; it never exposes an unauthenticated HTTP grant. An ambiguous handle fails instead of guessing. A public handle registered by someone else before bootstrap can be selected by mistake, so prefer `--credential` when more than one person can reach the instance.

## Production domain

1. Point the chosen hostname (for example `looklab.fujimod.com`) at the VM with an A/AAAA record. Do not change DNS for a hostname until it is the one you will keep: WebAuthn credentials are bound to the RP ID.
2. Terminate TLS at a reverse proxy on the same host and forward to `127.0.0.1:8080`, preserving the `Host` header. Keep `bind` on loopback when a proxy is present.
3. Set `origin = "https://looklab.fujimod.com"` and `rp_id = "looklab.fujimod.com"`.
4. Restart both units and verify `/readyz` from the public URL. The static SPA calls the API on its own origin; CORS stays empty.
5. If the SPA is hosted separately, add its origin to `cors_origins`, rebuild it with `LOOKLAB_API_BASE=https://api.<domain>`, and keep `origin`/`rp_id` on the page hostname.

Passkey RP IDs are permanent for existing credentials. Changing the hostname later forces every contributor and moderator to register a new passkey, so decide the production name before inviting anyone.

## Sizing

On a local debug build the server idles near 22 MB RSS and peaked at 174 MB after page loads and uploads; an idle worker uses about 15 MB. Worker memory scales with refit batch size: a synthetic 1,000-image Look refit in one batch reached about 2 GB RSS, so a 1 GB host suits small batches and larger refits need more memory. Both share one SQLite file; run exactly one worker process. The worker polls every 5 s, renews its job lease every third of `--lease-seconds`, and reclaims interrupted jobs after the lease expires. Queued jobs live in SQLite and survive restarts; the startup path re-queues requested publications and publishes a missing catalog.

The default worker solve uses the fitter's sweep limit. An explicit `WorkerConfig::sweep_budget` limits total sample evaluations and produces a resumable checkpoint on exhaustion. A fixed 2M-evaluation budget cannot reach final refinement for the 1,000-image fixture after warm-up.

## Readiness and smoke checks

```sh
curl -fsS http://127.0.0.1:8080/healthz
curl -fsS http://127.0.0.1:8080/readyz
curl -fsS http://127.0.0.1:8080/data/site-root.json
curl -fsS http://127.0.0.1:8080/data/index.json | head -c 200
sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' \
  "SELECT kind, status, COUNT(*) FROM jobs GROUP BY kind, status"
```

`/readyz` is 200 only when SQLite answers and, if `site.data_dir` is configured, the published index parses with format `looklab-site/1`. A missing index is reported as `site: missing`, an unparsable one as `site: invalid`.

To prove persistence and publication from the browser: register a passkey, upload one batch, and note the status URL. Restart both services, then reload `/status/<token>` (the SPA page) and its `.rss` feed; the feed must list the same contribution. The worker log or the `jobs` query must show the queued scoring and publication rows moving to `done`, and `/data/index.json` must keep the same digest unless the catalog changed.

## Containers

Run the server and worker as two processes of the same image, sharing one volume at `/var/lib/looklab`; do not run them in one container with a shared PID namespace. Pass the config file or the `LOOKLAB_*` variables, expose only the server port, and send SIGTERM to each process. The server drains in-flight HTTP requests and closes the pool; the worker finishes its current job and exits without claiming another. Do not run the worker on an ephemeral filesystem: SQLite, blobs and `site/data` must be on the persistent volume.

## Untouched constraints

Public intake still requires the legal review tracked in docs/10 Q3. Do not open registration to the public and do not create DNS, accounts or hosting from this repository.
