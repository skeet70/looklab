# Litestream replication and restore drill

`looklab-server` stores its entities in one SQLite database and opens it in WAL mode at connect ([03 §4](../03-data-model.md)). Litestream streams the WAL to a replica, so the recovery point is a few seconds, not the last nightly copy.

## Replication

Example `/etc/looklab/litestream.yml`:

```yaml
dbs:
  - path: /var/lib/looklab/looklab.sqlite3
    replicas:
      - type: s3
        bucket: looklab-sqlite
        path: looklab
        region: eu-central-1
        retention: 720h
        sync-interval: 1s
```

- Keep the database, its `-wal` and `-shm` files on a local filesystem. Litestream must see the WAL; the server sets `journal_mode=WAL`, `synchronous=FULL`, `foreign_keys=ON` and a 5 s busy timeout on every connection.
- One server process writes the database. Run Litestream beside it, not inside it.
- Litestream keeps its local state in `.looklab.sqlite3-litestream/`. Do not delete or restore over it while replication runs.
- `retention` is how far back a point-in-time restore can reach. 720h matches the 30-day blob retention used by withdrawal ([06 §4](../06-privacy-and-payload.md)).
- The replica holds contribution metadata and serial HMACs. Give the bucket the same access controls as the database; sample blobs live outside SQLite and are private.

## Restore drill

Run this against a scratch path, never over the live database. The drill proves the replica contains recent transactions and that the file passes SQLite's own check.

1. Restore the newest transaction to a scratch file:

```sh
litestream restore -config /etc/looklab/litestream.yml -o /tmp/looklab-restore-check.sqlite3 /var/lib/looklab/looklab.sqlite3
```

2. Check the restored file:

```sh
sqlite3 /tmp/looklab-restore-check.sqlite3 'PRAGMA integrity_check'
sqlite3 /tmp/looklab-restore-check.sqlite3 'SELECT COUNT(*) FROM credentials'
sqlite3 /tmp/looklab-restore-check.sqlite3 'SELECT COUNT(*) FROM contributions'
```

`PRAGMA integrity_check` must print `ok`. Compare the counts with the live database, read-only:

```sh
sqlite3 'file:/var/lib/looklab/looklab.sqlite3?mode=ro' 'SELECT COUNT(*) FROM contributions'
```

3. Optionally point a scratch server at the restored file (`LOOKLAB_DB_PATH=/tmp/looklab-restore-check.sqlite3`) and fetch one status feed.

4. Record the date, Litestream version, restored transaction id (from `litestream restore` output or the replica listing) and the counts. Investigate any error other than "no backups found".

## Recorded drill

Run on 2026-11-04 with Litestream 0.5.17 and SQLite 3.53.3, using a file replica under `/tmp/looklab-litestream-drill/replica`:

```sh
# Schema and WAL mode
sqlite3 /tmp/looklab-litestream-drill/looklab.sqlite3 < crates/looklab-server/migrations/0001_init.sql
sqlite3 /tmp/looklab-litestream-drill/looklab.sqlite3 'PRAGMA journal_mode=WAL;'
# wal

# Seed two credentials, one contribution and one contribution image, then start replication
litestream replicate -config /tmp/looklab-litestream-drill/litestream.yml
# level=INFO msg="initialized db" path=/tmp/looklab-litestream-drill/looklab.sqlite3
# level=INFO msg="replicating to" type=file sync-interval=1s path=/tmp/looklab-litestream-drill/replica

# While replication runs, insert two more credentials and one more image
sqlite3 /tmp/looklab-litestream-drill/looklab.sqlite3 'SELECT COUNT(*) FROM credentials;'
# 4

# Stop Litestream, then restore to a scratch file
litestream restore -config /tmp/looklab-litestream-drill/litestream.yml -o /tmp/looklab-litestream-drill/restored.sqlite3 /tmp/looklab-litestream-drill/looklab.sqlite3
sqlite3 /tmp/looklab-litestream-drill/restored.sqlite3 'PRAGMA integrity_check'
# ok
sqlite3 /tmp/looklab-litestream-drill/restored.sqlite3 'SELECT COUNT(*) FROM credentials'
# 4
sqlite3 /tmp/looklab-litestream-drill/restored.sqlite3 'SELECT COUNT(*) FROM contribution_images'
# 2
```

The WAL, `-shm` and Litestream state directory were present beside the live database during replication, and the replica contained an `ltx/` tree. The restored counts matched the writes made after replication started, and the integrity check passed.
