# Deployment

The production process model is two native binaries over one local state directory: `looklab-server` owns the HTTP listener, SQLite, private blobs and the published data directory; `looklab-worker` owns fits, trust jobs and static publication. Both open the same SQLite file. There is no queue broker, object store or external cache. (`docs/ops/litestream.md` covers database replication.)

A host is one NixOS configuration that imports `nixosModules.looklab`. The module packages the server, the worker, the operator CLI and the SPA, runs the two services under a dedicated user, terminates TLS in Caddy and seeds the data tree. `nixosConfigurations.looklab-ovh` is the production host; a staging host or a second provider is another entry that imports the same module.

## Modes

- **Single origin (recommended).** The server serves `/api/*`, `/healthz`, `/readyz`, `/data/*` and the SPA from `site.dist_dir` and `site.data_dir`. Set `origin`/`rp_id` to the public site hostname and leave `cors_origins` empty. Passkeys, fetches and the publication-root endpoint are all same-origin.
- **Split origin.** The SPA lives on a static host or CDN; the API serves the live catalog and publication artifacts on another hostname. Build the SPA with `LOOKLAB_API_BASE=https://api.example`, configure the API's `site.data_dir`, and list the page origin in `cors_origins`. Set `origin` and `rp_id` to the public page hostname, not the API hostname. No cookies are issued; contributor and moderator sessions are `Authorization: Bearer` tokens, so CORS never needs credentials or a wildcard.
- **API only.** Set `services.looklab.site.distDir = null` and `services.looklab.site.dataDir = null`, and list the page origin in `corsOrigins`. Static pages and `/data` must come from elsewhere, and the SPA needs a compiled site-root pin or a split-origin root endpoint.
- **Distribution with baked data.** Point `site.distDir` at a distribution that carries its own `data/index.json`, as the Pages workflow builds one. The server advertises the baked root and serves it through the SPA fallback. The packaged `looklab-site` ships no baked data, so this mode needs a separately built distribution.

The browser trusts the configured API origin for the publication root: it fetches `/data/site-root.json` and rejects any `index.json` whose SHA-256 differs. The serverless preview keeps the digest compiled into the wasm. Artifacts remain verified against their manifests and manifests against the index, so a changing worker catalog is browsable without rebuilding the SPA or weakening hash checks.

The GitHub Pages workflow stays a serverless preview by default. Set the repository variable `LOOKLAB_PUBLIC_API_BASE` to the public API origin (for example `https://api.looklab.example`) to build the split-origin page instead: the workflow then drops `LOOKLAB_PREVIEW`, compiles `LOOKLAB_API_BASE`, and the deployed page reads the live root and catalog through that API. Keep the API's `origin` and `rp_id` on the Pages hostname and add the Pages origin to `cors_origins`.

## State and configuration

| Path | Contents | Access |
|---|---|---|
| `/var/lib/looklab/looklab.sqlite3` (+ `-wal`, `-shm`) | credentials, grants, contributions, jobs, audit log | private, never served |
| `/var/lib/looklab/blobs` | uploaded sample blobs and artifact bytes | private, never served |
| `/var/lib/looklab/community` | approved versioned community snapshots and private publication staging | private, never served directly |
| `/var/lib/looklab/server-secret` | 32 random HMAC bytes, mode 0600 | private, never served |
| `/var/lib/looklab/data` | seeded `data/` tree: bodies, transforms, interfaces, probes, card conformance vectors | read-only to the worker |
| `/var/lib/looklab/data/manuals` | documentation-derived camera catalog, refreshed from `looklab-manuals` | read-only to the worker |
| `/var/lib/looklab/data/scenes` | prepared CC0 photo scene PNGs and their provenance index | read-only to the worker |
| `/var/lib/looklab/site` | worker publication root (`--site`) | worker writes, not served |
| `/var/lib/looklab/site/data` | published data tree | public under `/data/` |
| `/var/lib/looklab/site/site-root.txt` | index digest for static builds | not served |
| `/nix/store/…-looklab-site/dist` | built SPA from `site/scripts/build-site.sh`, `services.looklab.site.distDir` | public at `/` |
| `/var/lib/looklab-secrets/litestream.env` | Litestream credentials, mode 0600, root-only | never served, never in the store |

Both units set `StateDirectoryMode=0700` and `UMask=0077`; `deploy/run-local.sh` sets `umask 077` before creating its state directory, so new database, blob and secret files are owner-only. Do not chmod an existing project or user directory to enforce this; create a new state root instead.

Every worker publication writes `data/developers/conformance-v1.zip` and its SHA-256 metadata alongside the catalog, from the `data/card/conformance-v1` tree, so the live developer download works without the sample-site generator. A publication fails rather than omitting the download when that source tree is absent.

The module writes `/etc/looklab/looklab.toml` from the options (`bind`, `origin`, `rpId`, `corsOrigins`, `stateDir`, `dataDir`, `manualsDir`, `site.distDir`), seeds `data/` from the flake on first start, and creates `site/data`. `init` runs before the server starts: it creates the missing state directories, generates the server secret once and runs every SQLite migration. A fresh database has no published catalog; the worker's startup publishes one from `data/` even when there are no Looks yet, so the SPA can render an explicit empty state. `/readyz` returns 503 until that first `data/index.json` exists.

The worker receives `--snapshot-dir /var/lib/looklab/community`, the same root as the server's `snapshot_dir`. Schema publication starts from the full authoritative baseline, applies the approved proposal and publishes a new snapshot version; server intake refreshes its snapshot view without a restart. The proposal's publication transition and catalog-publication request commit in one transaction. Worker publication names that version in the site index and publishes body pages even when no Look fit succeeds. Interrupted publication resumes from its recorded version and content hash. A binary switch does not approve new camera mappings or replace the seeded body files.

The seeded data tree is a copy, not a store link: add a camera transform by writing `${dataDir}/transforms/<make>-<model>.yaml` as the `looklab` user. A re-install of the module does not overwrite it, and `rm ${dataDir}/.seeded` re-seeds from the flake on the next start.

The module refreshes `${manualsDir}` from `packages.<system>.looklab-manuals` on every seed-service start, including when `.seeded` exists. Packaged records replace same-named files; files that exist only on the host remain. The refresh does not touch `bodies`, `transforms` or `.seeded`.

Photographic preview scenes are prepared offline; the worker never fetches sources or decodes RAW. `packages.<system>.looklab-scenes` builds the directory: it fetches the six `scene-*.dng` fixtures named in `fixtures/manifest.toml` as fixed-output derivations (URL and SHA-256 from the manifest) and runs the packaged `prepare_scenes` over them. `services.looklab.scenesDir` defaults to that package, so a host publishes the photo scenes without a manual step.

The directory holds one 16-bit PNG per scene plus `index.json`, which records each scene's kind, title, source URL, license, source SHA-256 and PNG SHA-256. The module copies it into `${dataDir}/scenes` on first start, or when `scenes/index.json` is still absent. The worker verifies every PNG against that index and publishes the scenes with their provenance; a digest or size mismatch fails the publication. Without the directory it publishes the three test charts and logs a warning.

To prepare a directory from another manifest or cache, run the same example directly and point `services.looklab.scenesDir` at its output:

```sh
nix develop -c cargo run --locked -p looklab-publish --features photo-scenes --example prepare_scenes -- \
  --manifest fixtures/manifest.toml --cache fixtures/cache --out /var/lib/looklab-scenes
```

On a host that already published a catalog, seeding scenes or changing the manual catalog does not by itself trigger a new publication: the worker publishes at startup only when `data/index.json` is missing or the published index has no manuals. Publish once as the service user after the switch:

```sh
sudo -u looklab looklab publish --db /var/lib/looklab/looklab.sqlite3 --blobs /var/lib/looklab/blobs \
  --data /var/lib/looklab/data --manuals /var/lib/looklab/data/manuals \
  --snapshot-dir /var/lib/looklab/community \
  --transforms /var/lib/looklab/data/transforms --out /var/lib/looklab/site
```

Server-side colour fitting requires `data/transforms/<make>-<model>.yaml`. Browser preparation uses the RAW's local camera matrices, so a missing server prior does not block camera discovery or contribution admission. Local decoder, matrix and published control-schema checks still apply. The shipped X-S10 and X-T4 files contain DNG ColorMatrix endpoints extracted from the licensed or authorized local sources; they are input priors, not native Look measurements or root admissions.

For another camera, `looklab prep --raw <RAW> --jpeg <JPEG> --out <private-directory>` emits a `transform.yaml` from the RAW's DNG profile. Review its body and version, then install that file under the worker's transform directory with the body's filename. Keep the photographs, samples and sensitive seed file private. Scoring fails when a required camera prior is absent; it does not substitute an identity matrix.

The server creates the secret with OS randomness when the file is absent and keeps it stable across restarts. Changing it invalidates serial HMACs and every outstanding signed upload, so back it up with the database. Do not place the database, blob store, secret, `data/bodies` source tree or donation fixtures under `site.distDir`; only the worker output belongs at `/data/`.

`origin` must be the exact public page origin and `rp_id` its host or a parent domain; HTTP origins are accepted only for `localhost` and `127.0.0.1`. Nonce responses advertise the configured RP ID, and the SPA uses that value for assertions as well as the server's registration options.

## Hosts

`nixosConfigurations.looklab-ovh` ([nix/hosts/looklab-ovh.nix](../../nix/hosts/looklab-ovh.nix)) holds the OVH VPS-1 specifics: the disko layout for the single virtio disk (`/dev/vda`, GPT with a 1 MB BIOS boot partition, a 512 MB ESP and ext4 on the rest), a hybrid GRUB install that boots from either BIOS or UEFI, OpenSSH key-only, automatic garbage collection and the module itself:

```nix
services.looklab = {
  enable = true;
  hostname = "looklab.fujimod.com";
  caddy.acmeEmail = null;          # owner supplies the ACME account email
  litestream.enable = false;       # enabled once the replica exists
  worker.memoryMax = "2.5G";
  worker.memoryHigh = "2G";
};
```

`hostname` sets the Caddy site and the default `origin` (`https://${hostname}`) and `rpId`. `users.users.root.openssh.authorizedKeys.keys` currently lists this workstation's public keys; replace the list with the owner's.

The worker has no CLI switch for refit batch size; peak memory tracks the batch a job carries, and `worker.memoryMax` / `worker.memoryHigh` bound it with cgroup limits. `worker.leaseSeconds` and `worker.seed` map to `--lease-seconds` and `--seed`. The VPS-1 has 2 vCPU and 3825 MiB, so the shipped limits are 2.5G max and 2G high: a 1,000-image refit in one batch does not fit, and needs small batches or VPS-2.

Adding a staging host is a new file plus one entry:

```nix
# nix/hosts/looklab-staging.nix
{ ... }: {
  imports = [ ];
  system.stateVersion = "26.11";
  networking.hostName = "looklab-staging";
  # disko layout for that provider's disk, boot loader, OpenSSH, firewall
  services.looklab = {
    enable = true;
    hostname = "staging.looklab.example";
    caddy.acmeEmail = "ops@example";
  };
}
```

and in `flake.nix`:

```nix
nixosConfigurations.looklab-staging = nixpkgs.lib.nixosSystem {
  system = "x86_64-linux";
  modules = [
    disko.nixosModules.disko
    self.nixosModules.looklab
    ./nix/hosts/looklab-staging.nix
  ];
};
```

Both hosts keep their own state and passkey RP IDs. Credentials are bound to the RP ID, so a staging host needs its own hostname and its own first admin.

## Install from Debian 13

The VPS arrives with Debian 13 and a root SSH login. The install kexecs into a NixOS installer, partitions the disk with the flake's disko layout, copies the system closure and reboots. **It wipes the target disk**, so only run it against the fresh VPS, never against a host that holds data.

```sh
# 1. DNS for the hostname already points at the VPS, or at least the IP is known.
# 2. The workstation can reach root@<ip> with a key.
nix run github:nix-community/nixos-anywhere -- \
  --flake .#looklab-ovh \
  --build-on local \
  --target-host root@<ip> \
  -i ~/.ssh/id_ed25519
```

`--build-on local` builds the closure here and copies it, which needs the looklab packages built once on the workstation. Omit `-i` when the agent already offers the key. To exercise the disk layout in a VM without touching the VPS:

```sh
nix run github:nix-community/nixos-anywhere -- --flake .#looklab-ovh --vm-test
```

After the reboot, `ssh root@<ip>` lands on the new system, and `systemctl status looklab-server looklab-worker caddy` shows the three units.

## Deploy and roll back

Build and copy from the workstation; the VPS does not need a compiler:

```sh
nixos-rebuild switch --flake .#looklab-ovh \
  --target-host root@<ip> \
  --build-host localhost
```

`nixos-rebuild` builds every changed derivation locally, copies the closure over SSH, switches the profile and restarts the units whose definitions changed. The server drains in-flight requests and the worker finishes its current job on `SIGTERM`, so a switch during a job delays the restart by up to the worker's `TimeoutStopSec`.

Roll back a bad switch without rebuilding:

```sh
ssh root@<ip> 'nix-env --rollback -p /nix/var/nix/profiles/system \
  && /nix/var/nix/profiles/system/bin/switch-to-configuration switch'
```

GRUB keeps the previous generations on the ESP; `boot.loader.timeout = 5` gives five seconds at boot to pick an older entry. A rollback restores binaries, the unit files and the generated config, but not the state directory, so a migration that already ran stays applied. Check database migration compatibility before starting an older binary.

Routine deployments do not require a new provider snapshot or a manual SQLite backup. Recovery relies on the automatic daily VPS backups, retained NixOS generations and rebuilding derived data. Restoring a daily snapshot can lose changes since that snapshot; that recovery point is accepted for routine deployments.

## Secrets

- **Server secret.** `/var/lib/looklab/server-secret`, mode 0600, created by `looklab-server init` on first start. The daily VPS backup includes it; keep a copy outside the VPS as well, because losing it invalidates serial HMACs and every outstanding signed upload.
- **Litestream credentials.** `/var/lib/looklab-secrets/litestream.env`, mode 0600, owned by root, read by systemd as `EnvironmentFile` and never by a Nix expression:

  ```
  LITESTREAM_ACCESS_KEY_ID=…
  LITESTREAM_SECRET_ACCESS_KEY=…
  ```

  The module creates the directory 0700 and the file's path is `services.looklab.litestream.envFile`; write the file by hand after the install. No secret belongs in the flake, the store or `configuration.nix`.
- **ACME account.** `services.looklab.caddy.acmeEmail` registers the Caddy ACME account; it is an email, not a credential.

## First admin

Registration creates a credential on first use; it has no roles. Grant the first admin offline through the audited CLI actor, which writes the grant and its audit entry in one transaction:

1. Open the site, create a profile with a display handle you control, and complete the passkey prompt.
2. Find the internal credential id when you do not want to match by handle:

```sh
sudo -u looklab sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' \
  "SELECT id, display_handle FROM credentials WHERE status='active' ORDER BY created_at DESC LIMIT 5"
```

3. Run the packaged CLI as the service user:

```sh
sudo -u looklab looklab admin grant \
  --db /var/lib/looklab/looklab.sqlite3 \
  --subject <internal-id> \
  --role admin \
  --reason "first admin bootstrap"
```

`services.looklab.cli.package` installs that binary on the host; it opens the database directly, so it runs as `looklab` beside the running server.

`looklab-server bootstrap-admin --config /etc/looklab/looklab.toml (--credential <id> | --display-handle <name>)` is the equivalent path inside the server binary; `--display-handle` is accepted when exactly one active credential carries it and fails on an ambiguous handle. A public handle registered by someone else before bootstrap can be selected by mistake, so prefer `--subject`/`--credential` when more than one person can reach the instance. The grant never goes through an unauthenticated HTTP route.

## Backups and Litestream

Production relies on OVH's automatic daily VPS backup. It snapshots the whole disk, so the state directory (database, blobs, server secret, publication tree) and `/etc` are covered; a restore runs from the OVH control panel onto the same VPS or its replacement. The recovery point is the last daily snapshot, and the restore replaces everything written since then, including Litestream's metadata if that is enabled.

Litestream remains the documented upgrade path when a finer recovery point is needed. It replicates the SQLite database every second to an S3-compatible replica and restores only the database, not blobs, the secret or the published site tree. It is off by default; enable it in the host config once the bucket exists:

```nix
services.looklab.litestream = {
  enable = true;
  url = "s3://looklab-sqlite/looklab";
  region = "eu-central-1";
  endpoint = null;                 # set for an S3-compatible provider
};
```

The module writes `/etc/looklab/litestream.yml` (0.5 schema: `dbs`, `snapshot`, `l0-retention`, `socket`) and runs `litestream replicate` as a third unit beside the server and worker. `docs/ops/litestream.md` covers WAL checkpointing with three processes, retention and the drill.

Restore onto the host, with both application units and replication stopped:

```sh
systemctl stop looklab-litestream looklab-worker looklab-server
cd /var/lib/looklab
mv looklab.sqlite3 looklab.sqlite3.lost
mv looklab.sqlite3-wal looklab.sqlite3-wal.lost 2>/dev/null || true
mv looklab.sqlite3-shm looklab.sqlite3-shm.lost 2>/dev/null || true
sudo -u looklab litestream restore -config /etc/looklab/litestream.yml \
  -integrity-check full -o /var/lib/looklab/looklab.sqlite3 s3://looklab-sqlite/looklab
sudo -u looklab litestream reset -config /etc/looklab/litestream.yml /var/lib/looklab/looklab.sqlite3
systemctl start looklab-server looklab-worker looklab-litestream
curl -fsS http://127.0.0.1:8080/readyz
```

`litestream reset` drops the metadata for the replaced file so the next sync writes a fresh snapshot instead of continuing the old chain. The replica holds contribution metadata, serial HMACs and WebAuthn credentials; blobs, the server secret and the published site tree are not covered and need their own backups.

## Local runs

For a local production-shaped run without systemd:

```sh
deploy/run-local.sh
```

It stores state under `${XDG_STATE_HOME:-$HOME/.local/state}/looklab`, incrementally builds development binaries and the SPA from captured source generations, serves <http://localhost:8080>, and starts the worker before the server. Override the port with `LOOKLAB_PORT` and the state root with `LOOKLAB_STATE_DIR`. Set `LOOKLAB_DATA_DIR` to launch from another `data/` tree, for example one with prepared scenes under `scenes/`; that tree also supplies the bodies and transforms. Installed production services use the release binaries.

## Sizing

On a local debug build the server idles near 22 MB RSS and peaked at 174 MB after page loads and uploads; an idle worker uses about 15 MB. Worker memory scales with refit batch size: a synthetic 1,000-image Look refit in one batch reached about 2 GB RSS, so a 1 GB host suits small batches and larger refits need more memory. Both share one SQLite file; run exactly one worker process. The worker polls every 5 s, renews its job lease every third of `--lease-seconds`, and reclaims interrupted jobs after the lease expires. Queued jobs live in SQLite and survive restarts; the startup path re-queues requested publications and publishes a missing catalog.

The default worker solve uses the fitter's sweep limit. An explicit `WorkerConfig::sweep_budget` limits total sample evaluations and produces a resumable checkpoint on exhaustion. A fixed 2M-evaluation budget cannot reach final refinement for the 1,000-image fixture after warm-up.

## Browser error diagnostics

For a reported web-app failure, read the private recent reports on the host:

```sh
sudo -u looklab looklab-server client-errors \
  --config /etc/looklab/looklab.toml --limit 50
sudo -u looklab looklab-server client-errors \
  --config /etc/looklab/looklab.toml --limit 100 --json --release <64-lowercase-hex-build-id>
```

The command opens the existing database read-only, without migrations. Default limit is 50, accepted range is 1 to 1000, newest first. Output includes UTC receipt time, build SHA-256, route category, error category/type and application asset locations. For WASM, the displayed line and column are function index and byte offset. No error text is collected: use the location and the matching build to find the failing code. Multiple incidents at one location can be deduplicated within a page; report counts are not visitor counts.

The page's `script[src$="errors.js"]` carries `data-release`, a SHA-256 over the page template and emitted application JS/WASM. Preserve the distribution for a deployed build when investigating it. A rebuild with different emitted bytes has a different identifier; the identifier is not a Git revision or an integrity signature.

Intake is `POST /api/client-errors`, 204 on acceptance, 400 on invalid schema, 408 when the configured `body_timeout_seconds` expires, 413 over 16 KiB, 415 without JSON media type and 429 when its global bucket is exhausted. No public GET exists. Requests carry no session token, cookies or referrer; reports contain no exception messages or console arguments. Do not add request-body, IP or authorization logging to troubleshoot this channel.

The private `client_errors` table holds at most 10,000 reports. Inserts delete rows older than 14 days and enforce the count cap atomically; read-only retrieval filters the same time window. On an idle service, expired rows are physically removed by the next insert, not by the CLI. Backups follow their own expiry policy. Serverless previews disable reporting; split-origin builds use `LOOKLAB_API_BASE` and the configured CORS origins.

Run the browser boundary check without a running application or photo fixtures:

```sh
nix build --no-link nixpkgs#playwright-driver nixpkgs#playwright-driver.browsers
nix develop -c env \
  PLAYWRIGHT_MODULE="$(nix eval --raw nixpkgs#playwright-driver)/index.mjs" \
  PLAYWRIGHT_BROWSERS_PATH="$(nix eval --raw nixpkgs#playwright-driver.browsers)" \
  node site/checks/client-errors.mjs
```

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

`nix build .#checks.x86_64-linux.looklab-deploy -L` boots the module in a VM, waits for both units, checks `/healthz`, `/readyz`, `/`, `/data/index.json`, the state-dir and secret permissions, then restarts both units and checks that the secret, the published index and the migrated database survive. The published index must list the six photo scenes and the three test charts with CC0 provenance, and a scene PNG must match its listed digest.

To prove persistence and publication from the browser: register a passkey, upload one batch, and note the status URL. Restart both services, then reload `/status/<token>` (the SPA page) and its `.rss` feed; the feed must list the same contribution. The worker log or the `jobs` query must show the queued scoring and publication rows moving to `done`, and `/data/index.json` must keep the same digest unless the catalog changed.

## Containers

Run the server and worker as two processes of the same image, sharing one volume at `/var/lib/looklab`; do not run them in one container with a shared PID namespace. Pass the config file or the `LOOKLAB_*` variables, expose only the server port, and send SIGTERM to each process. The server drains in-flight HTTP requests and closes the pool; the worker finishes its current job and exits without claiming another. Do not run the worker on an ephemeral filesystem: SQLite, blobs and `site/data` must be on the persistent volume. `deploy/looklab.toml.example` lists every config key for a hand-written file.

## Untouched constraints

Public intake still requires the legal review tracked in docs/10 Q3. Do not open registration to the public and do not create DNS, accounts or hosting from this repository.
