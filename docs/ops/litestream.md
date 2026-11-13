# Litestream replication and restore drill

`looklab-server`, the fit `looklab-worker` and `looklab-publisher` share one SQLite database in WAL mode ([03 §4](../03-data-model.md)). Litestream 0.5 runs beside them as a fourth process, reads committed WAL pages and writes LTX files to a replica. Replication is asynchronous, so the recovery point is the last sync, not the last commit. Production currently relies on OVH's automatic daily VPS backup instead ([deployment.md](deployment.md)); Litestream is the upgrade path for a finer recovery point and is disabled by default.

## Replication

On a NixOS host `services.looklab.litestream` generates `/etc/looklab/litestream.yml` from the module options. [deploy/litestream.yml.example](../../deploy/litestream.yml.example) is the same 0.5 schema for a hand-written file:

```yaml
dbs:
  - path: /var/lib/looklab/looklab.sqlite3
    replica:
      url: s3://looklab-sqlite/looklab
      region: eu-central-1
      sync-interval: 1s

snapshot:
  interval: 24h
  retention: 720h
l0-retention: 24h

socket:
  enabled: true
  path: /var/lib/looklab/litestream.sock
```

- This is the 0.5 schema. The 0.3 schema uses a `replicas:` array and a per-replica `retention`; in 0.5 retention is the global `snapshot` and `l0-retention` configuration shown here. Check `litestream version` before copying a config.
- Credentials come from `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`, or from `LITESTREAM_ACCESS_KEY_ID` / `LITESTREAM_SECRET_ACCESS_KEY`. Keep them out of the config file.
- For an S3-compatible endpoint add `endpoint:` and `force-path-style: true` under `replica:`. The local drill replaces the whole `replica:` block with `url: file:///tmp/looklab-litestream-drill/replica`.
- `snapshot.retention` bounds how far back a restore can reach; 720h matches the 30-day blob retention used by withdrawal ([06 §4](../06-privacy-and-payload.md)). `l0-retention` bounds how long the finest, per-sync restore points survive.
- `retention.enabled` defaults to true and lets Litestream delete expired files from the replica. Set it to false when the credentials must not have delete permission, and expire objects with a bucket lifecycle rule instead.
- Run one Litestream per database and replica path. Two instances writing the same path leave a chain that cannot be restored.
- The replica holds contribution metadata, serial HMACs and WebAuthn credentials. Give the bucket the same access controls as the database. Sample blobs, the server secret and the published site tree live outside SQLite and are not covered by Litestream.

## WAL and checkpointing with the application processes

Four processes open the file: `looklab-server`, the fit `looklab-worker`, `looklab-publisher` and Litestream. The application processes write; Litestream reads the WAL and holds a read transaction between its own checkpoints so that no other process can restart the WAL underneath it. Litestream then checkpoints the database itself: PASSIVE when the WAL passes `min-checkpoint-page-count` (1000 pages) or `checkpoint-interval` (1m), and a TRUNCATE when it passes `truncate-page-n` (121359 pages, about 500 MB).

- All application processes use the same `db::open` (`looklab_server::open_database`), which sets `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON` and a 5 s busy timeout on every connection (`crates/looklab-server/src/db.rs`). The busy timeout absorbs Litestream's short checkpoint write locks.
- A forced TRUNCATE checkpoint waits for all readers and writers. On a WAL near the 500 MB threshold that wait can exceed the 5 s busy timeout and fail a request. The database holds metadata only, with sample blobs outside SQLite; do not run `VACUUM` or `wal_checkpoint(TRUNCATE)` by hand during traffic.
- The application does not set `wal_autocheckpoint=0`; SQLite's default autocheckpoint at 1000 pages stays active. A checkpoint by another connection between a write and its sync is detected by 0.5: the drill forced `wal_checkpoint(TRUNCATE)` after a write and before the sync, the WAL went from 70072 bytes to 0, and the next sync still wrote TXID 2 (191 bytes) and the restored file contained the row. Under sustained high write rates the repeated app checkpoints make Litestream fall back to full-state snapshots; if the database ever becomes write-heavy, open the application connections with `wal_autocheckpoint=0` and let Litestream own checkpointing.
- Keep the database, its `-wal` and `-shm` files on a local filesystem, and keep Litestream's metadata directory `.looklab.sqlite3-litestream/` beside the database. Do not delete or replace any of them under a running server.
- Do not copy the `.sqlite3` file alone while the WAL has frames; the copy misses committed transactions. Use `litestream restore`, or `VACUUM INTO` on a quiesced database.
- If the database is deleted and recreated, remove all three files and run `litestream reset <db>`. Litestream does not track database deletions.
- Never restore over the live path while the server, either worker unit or Litestream runs; `litestream restore` refuses to overwrite without `-force`. After replacing the file in place, run `litestream reset <db>` before replication resumes so Litestream takes a fresh snapshot instead of continuing from metadata for the old file.
- Replication is asynchronous. The recovery point is `sync-interval` plus the in-flight window; a graceful stop flushes with a final sync (`shutdown-sync-timeout`, 30 s default), and a host loss loses the last window.

## Restore drill

The drill creates a throwaway state directory, initializes the schema with `looklab-server init`, writes rows before and during replication, forces a checkpoint between a write and its sync, replicates, moves the database and its sidecars aside, restores, checks integrity and counts, and starts the server against the restored file. The rows are direct SQL inserts against the migrated schema, not uploads through the WebAuthn and proof-of-work API. The drill uses a local file replica so it needs no credentials or network.

```sh
nix shell nixpkgs#litestream nixpkgs#sqlite nixpkgs#curl -c env \
  LITESTREAM=litestream SQLITE=sqlite3 \
  tools/litestream-drill.sh
```

The script builds nothing; build the server first with `nix develop -c tools/build.sh --lane native -- cargo build --locked -p looklab-server`. Override `LOOKLAB_SERVER`, `DRILL_PORT` (default 8091), or `DRILL_KEEP=1` to keep the state directory. On success it removes the state directory.

For a production replica, run the same checks by hand against a scratch file:

```sh
litestream restore -config /etc/looklab/litestream.yml -json -integrity-check full \
  -o /tmp/looklab-restore-check.sqlite3 /var/lib/looklab/looklab.sqlite3
sqlite3 /tmp/looklab-restore-check.sqlite3 'PRAGMA integrity_check'
sqlite3 /tmp/looklab-restore-check.sqlite3 'PRAGMA foreign_key_check'
sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' \
  "SELECT (SELECT COUNT(*) FROM credentials), (SELECT COUNT(*) FROM contributions),
          (SELECT COUNT(*) FROM contribution_images)"
sqlite3 /tmp/looklab-restore-check.sqlite3 \
  "SELECT (SELECT COUNT(*) FROM credentials), (SELECT COUNT(*) FROM contributions),
          (SELECT COUNT(*) FROM contribution_images)"
```

`PRAGMA integrity_check` must print `ok` and `foreign_key_check` must print nothing. Remove a previous scratch file first; restore refuses to overwrite one unless `-force`. Run the restore as the `looklab` user, or chown the scratch file, so the scratch server can read it. Then point a scratch server at the restored file and fetch readiness:

```sh
sudo -u looklab env LOOKLAB_DB_PATH=/tmp/looklab-restore-check.sqlite3 \
  LOOKLAB_BIND=127.0.0.1:8091 /usr/local/bin/looklab-server --config /etc/looklab/looklab.toml
curl -fsS http://127.0.0.1:8091/readyz
```

Record the date, `litestream version`, SQLite version, the restored TXID from the `-json` output and the row counts. Investigate any error other than "no backups found". Delete the scratch file, and its `-wal`/`-shm` sidecars, after the server stops.

Restore points are whole LTX files: `-txid` is inclusive, `-timestamp` exclusive, and a target inside a file is skipped. `litestream ltx -level all <db>` lists the stored files and `litestream restore -dry-run` shows the plan; a listed TXID still fails when the chain below it is broken. With the example's `l0-retention: 24h`, per-sync points survive a day and older points are compaction boundaries until `snapshot.retention` removes them.

## Recorded drill

Run on 2026-11-06 on Linux x86_64 with Litestream 0.5.17, SQLite 3.53.3 and the debug `looklab-server` from this checkout, using a file replica. Output trimmed to the evidence:

```
$ nix shell nixpkgs#litestream nixpkgs#sqlite nixpkgs#curl -c env LITESTREAM=litestream SQLITE=sqlite3 tools/litestream-drill.sh
== initialize the database through looklab-server init ==
initialised /tmp/looklab-litestream-drill/looklab.sqlite3
journal_mode: wal
migrations applied: 18

== start replication ==
txid: 1
replica_txid: 1
ltx: 0      0000000000000001  0000000000000001  22556  2026-11-06T18:43:08Z

== write the second batch, checkpoint from a second connection, then replicate ==
wal before checkpoint: 70072 bytes
PRAGMA wal_checkpoint(TRUNCATE): 0|0|0
wal after checkpoint: 0 bytes
txid: 2
replica_txid: 2
ltx: 0      0000000000000002  0000000000000002  191    2026-11-06T18:43:08Z

== write the third batch after the checkpoint and replicate it ==
txid: 3
replica_txid: 3
ltx: 0      0000000000000003  0000000000000003  2964   2026-11-06T18:43:08Z
expected rows: 5 credentials, 5 contributions, 7 contribution_images, 4 audit_log rows

== stop replication and simulate loss of the live database ==
moved /tmp/looklab-litestream-drill/looklab.sqlite3 and its sidecars to *.lost; replica retained at /tmp/looklab-litestream-drill/replica
replica size: 32K	/tmp/looklab-litestream-drill/replica

== restore to a scratch file and verify ==
level=INFO msg="post-restore integrity check passed" db=looklab.sqlite3 replica=file
{
  "db_path": "/tmp/looklab-litestream-drill/restored.sqlite3",
  "replica": "file",
  "txid": "0000000000000003",
  "duration_ms": 5,
  "integrity_check": "full"
}
integrity_check: ok
foreign_key_check: ok (no rows)
restored rows: 5 credentials, 5 contributions, 7 contribution_images, 4 audit_log rows

== serve the restored database on 127.0.0.1:8091 ==
/readyz: {"status":"ok","database":"ok","site":"not_configured"}
/healthz: {"status":"ok"}
```

The drill is API-only, so `site` reports `not_configured`. A production restore should also see `site: ok` once the worker's published `index.json` is present.

## What the drill proves

- Writes made before replication, after replication started, and after a foreign checkpoint all reach the replica (TXIDs 1 to 3).
- A checkpoint by another connection between a write and its sync does not lose the write; Litestream captures the page from the main database file on the next sync.
- `litestream restore` rebuilds a single database file that passes the full post-restore integrity check, `PRAGMA integrity_check` and `PRAGMA foreign_key_check`.
- The restored row counts equal the live counts at the point of loss.
- `looklab-server` runs its migrations and serves `/readyz` with `database: ok` against the restored file.

## What it does not prove

- S3 credentials, bucket policy, object lifecycle or cross-host durability. The drill uses a file replica; the S3 example is accepted by `litestream databases` but not exercised here.
- Recovery of blobs, the server secret or the published site tree. Those need their own backups. A restored database served with a different `server-secret` invalidates serial HMACs and every outstanding signed upload.
- Point-in-time restore to an older TXID, and retention expiry. Both need a longer run and a configured `snapshot.retention`.
- A hot standby. The restored file is opened by a freshly started server after the application processes and Litestream have stopped.
