#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Backup and restore drill for the looklab SQLite database against a local file
# replica. See docs/ops/litestream.md.
#
# Usage: tools/litestream-drill.sh [state-dir]
#
# Environment:
#   LOOKLAB_SERVER   server binary (default: <repo>/target/debug/looklab-server)
#   LITESTREAM       litestream binary (default: litestream from PATH)
#   SQLITE           sqlite3 binary (default: sqlite3 from PATH)
#   DRILL_PORT       port for the restored server (default: 8091)
#   DRILL_KEEP=1     keep the state directory after a successful run
set -euo pipefail

root=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
state=${1:-/tmp/looklab-litestream-drill}
[[ -n $state && $state != / ]] || { printf 'refusing to use %s as the drill state\n' "$state" >&2; exit 2; }
port=${DRILL_PORT:-8091}
server=${LOOKLAB_SERVER:-$root/target/debug/looklab-server}
litestream=${LITESTREAM:-litestream}
sqlite=${SQLITE:-sqlite3}

db=$state/looklab.sqlite3
replica=$state/replica
restored=$state/restored.sqlite3
cfg=$state/litestream.yml
sock=$state/litestream.sock
litestream_pid=
server_pid=
failed=0

step() { printf '\n== %s ==\n' "$*"; }
die() { printf 'drill failed: %s\n' "$*" >&2; failed=1; exit 1; }

cleanup() {
  local status=${1:-$?}
  trap - EXIT INT TERM
  if [[ -n $server_pid ]]; then kill "$server_pid" 2>/dev/null || true; wait "$server_pid" 2>/dev/null || true; fi
  if [[ -n $litestream_pid ]]; then kill "$litestream_pid" 2>/dev/null || true; wait "$litestream_pid" 2>/dev/null || true; fi
  if [[ $status -eq 0 && $failed -eq 0 && ${DRILL_KEEP:-0} != 1 ]]; then
    rm -rf "$state"
  else
    printf 'drill artifacts kept in %s\n' "$state" >&2
  fi
  exit "$status"
}
trap 'cleanup $?' EXIT
trap 'cleanup 130' INT
trap 'cleanup 143' TERM

for bin in "$server" "$litestream" "$sqlite" curl; do
  command -v "$bin" >/dev/null 2>&1 || die "$bin not found; build the server or enter the drill shell"
done

sql() { "$sqlite" -bail -cmd '.timeout 5000' "$1" "$2"; }

counts() {
  "$sqlite" -bail -readonly "$1" \
    "SELECT
       (SELECT COUNT(*) FROM credentials) || ' credentials, ' ||
       (SELECT COUNT(*) FROM contributions) || ' contributions, ' ||
       (SELECT COUNT(*) FROM contribution_images) || ' contribution_images, ' ||
       (SELECT COUNT(*) FROM audit_log) || ' audit_log rows'"
}

write_batch_one() {
  sql "$db" "
    BEGIN IMMEDIATE;
    INSERT INTO credentials (id, credential_id, webauthn_credential, display_handle, tier, roles, reputation, created_at, status) VALUES
      ('11111111-1111-4111-8111-111111111111', X'0101', '{\"v\":1,\"handle\":\"drill-alice\"}', 'drill-alice', 'new', '[]', 0.5, 1762300000, 'active'),
      ('22222222-2222-4222-8222-222222222222', X'0102', '{\"v\":1,\"handle\":\"drill-bob\"}', 'drill-bob', 'endorsed', '[]', 0.6, 1762300060, 'active'),
      ('33333333-3333-4333-8333-333333333333', X'0103', '{\"v\":1,\"handle\":\"drill-carol\"}', 'drill-carol', 'new', '[]', 0.5, 1762300120, 'active');
    INSERT INTO contributions (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent, target_look_id, body_id, firmware_version, sim_key, settings, status, created_at) VALUES
      ('a1111111-1111-4111-8111-111111111111', '11111111-1111-4111-8111-111111111111', X'aa01', X'bb01', '{\"memory_kib\":65536,\"iterations\":3}', 'library', X'cc01', 'fit', NULL, 'fujifilm-x-t4', '2.10', 'x-t4-210', '{\"film_simulation\":\"classic_chrome\"}', 'accepted', 1762300200),
      ('a2222222-2222-4222-8222-222222222222', '22222222-2222-4222-8222-222222222222', X'aa02', X'bb02', '{\"memory_kib\":65536,\"iterations\":3}', 'rerender', NULL, 'fit', NULL, 'fujifilm-x-t4', '2.10', 'x-t4-210', '{\"film_simulation\":\"pro_neg_hi\"}', 'pending', 1762300260);
    INSERT INTO contribution_images (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status) VALUES
      ('b1111111-1111-4111-8111-111111111111', 'a1111111-1111-4111-8111-111111111111', X'dd01', 4096, '{\"kind\":\"library\"}', NULL, 'accepted'),
      ('b2222222-2222-4222-8222-222222222222', 'a1111111-1111-4111-8111-111111111111', X'dd02', 4096, '{\"kind\":\"library\"}', NULL, 'accepted'),
      ('b3333333-3333-4333-8333-333333333333', 'a2222222-2222-4222-8222-222222222222', X'dd03', 4096, '{\"kind\":\"rerender\"}', 'g1', 'pending');
    INSERT INTO audit_log (type, payload, ts) VALUES
      ('drill.seed', '{\"batch\":1,\"source\":\"litestream-drill\"}', 1762300300),
      ('drill.seed', '{\"batch\":1,\"source\":\"litestream-drill\"}', 1762300301);
    COMMIT;"
}

write_batch_two() {
  sql "$db" "
    BEGIN IMMEDIATE;
    INSERT INTO credentials (id, credential_id, webauthn_credential, display_handle, tier, roles, reputation, created_at, status) VALUES
      ('44444444-4444-4444-8444-444444444444', X'0104', '{\"v\":1,\"handle\":\"drill-dave\"}', 'drill-dave', 'new', '[]', 0.5, 1762300400, 'active');
    INSERT INTO contributions (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent, target_look_id, body_id, firmware_version, sim_key, settings, status, created_at) VALUES
      ('a3333333-3333-4333-8333-333333333333', '33333333-3333-4333-8333-333333333333', X'aa03', X'bb03', '{\"memory_kib\":65536,\"iterations\":3}', 'chart', X'cc03', 'fit', NULL, 'fujifilm-x-t4', '2.10', 'x-t4-210', '{\"film_simulation\":\"standard\"}', 'accepted', 1762300500),
      ('a4444444-4444-4444-8444-444444444444', '44444444-4444-4444-8444-444444444444', X'aa04', X'bb04', '{\"memory_kib\":65536,\"iterations\":3}', 'library', X'cc04', 'fit', NULL, 'fujifilm-x-t4', '2.10', 'x-t4-210', '{\"film_simulation\":\"acros\"}', 'pending', 1762300560);
    INSERT INTO contribution_images (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status) VALUES
      ('b4444444-4444-4444-8444-444444444444', 'a3333333-3333-4333-8333-333333333333', X'dd04', 4096, '{\"kind\":\"chart\"}', NULL, 'accepted'),
      ('b5555555-5555-4555-8555-555555555555', 'a4444444-4444-4444-8444-444444444444', X'dd05', 4096, '{\"kind\":\"library\"}', NULL, 'pending'),
      ('b6666666-6666-4666-8666-666666666666', 'a4444444-4444-4444-8444-444444444444', X'dd06', 4096, '{\"kind\":\"library\"}', NULL, 'pending');
    INSERT INTO audit_log (type, payload, ts) VALUES
      ('drill.seed', '{\"batch\":2,\"source\":\"litestream-drill\"}', 1762300600);
    COMMIT;"
}

write_batch_three() {
  sql "$db" "
    BEGIN IMMEDIATE;
    INSERT INTO credentials (id, credential_id, webauthn_credential, display_handle, tier, roles, reputation, created_at, status) VALUES
      ('55555555-5555-4555-8555-555555555555', X'0105', '{\"v\":1,\"handle\":\"drill-erin\"}', 'drill-erin', 'new', '[]', 0.5, 1762300700, 'active');
    INSERT INTO contributions (id, credential_id, batch_manifest_hash, signature, pow, kind, serial_hmac, intent, target_look_id, body_id, firmware_version, sim_key, settings, status, created_at) VALUES
      ('a5555555-5555-4555-8555-555555555555', '55555555-5555-4555-8555-555555555555', X'aa05', X'bb05', '{\"memory_kib\":65536,\"iterations\":3}', 'gradient', X'cc05', 'fit', NULL, 'fujifilm-x-t4', '2.10', 'x-t4-210', '{\"film_simulation\":\"monochrome\"}', 'pending', 1762300800);
    INSERT INTO contribution_images (id, contribution_id, samples_hash, n_samples, meta, rerender_group, status) VALUES
      ('b7777777-7777-4777-8777-777777777777', 'a5555555-5555-4555-8555-555555555555', X'dd07', 4096, '{\"kind\":\"gradient\"}', NULL, 'pending');
    INSERT INTO audit_log (type, payload, ts) VALUES
      ('drill.seed', '{\"batch\":3,\"source\":\"litestream-drill\"}', 1762300900);
    COMMIT;"
}

step "prepare throwaway state in $state"
rm -rf "$state"
mkdir -p "$state"
printf 'looklab state: %s\n' "$state"
printf 'server: %s\n' "$server"
printf 'litestream: %s\n' "$("$litestream" version)"

unset LOOKLAB_SITE_DIST LOOKLAB_SITE_DATA LOOKLAB_CORS_ORIGINS
export LOOKLAB_DB_PATH=$db
export LOOKLAB_BLOB_DIR=$state/blobs
export LOOKLAB_BODIES_DIR=$root/data/bodies
export LOOKLAB_SNAPSHOT_DIR=$state/community
export LOOKLAB_SERVER_SECRET_PATH=$state/server-secret
export LOOKLAB_RP_ID=localhost
export LOOKLAB_ORIGIN=http://localhost:$port
export LOOKLAB_BIND=127.0.0.1:$port

step "initialize the database through looklab-server init"
"$server" init
"$sqlite" -bail -readonly "$db" 'PRAGMA journal_mode;' | sed 's/^/journal_mode: /'
"$sqlite" -bail -readonly "$db" "SELECT COUNT(*) FROM _sqlx_migrations;" | sed 's/^/migrations applied: /'

step "write the first batch before replication starts"
write_batch_one
counts "$db" | sed 's/^/live: /'

cat >"$cfg" <<EOF
logging:
  level: info
dbs:
  - path: $db
    checkpoint-interval: 1m
    replica:
      url: file://$replica
socket:
  enabled: true
  path: $sock
EOF
printf 'config: %s\n' "$cfg"
sed 's/^/  /' "$cfg"

step "start replication"
"$litestream" replicate -config "$cfg" >"$state/litestream.log" 2>&1 &
litestream_pid=$!
for _ in $(seq 1 100); do
  [[ -S $sock ]] && break
  kill -0 "$litestream_pid" 2>/dev/null || die "litestream exited during startup"
  sleep 0.1
done
[[ -S $sock ]] || die "litestream control socket did not appear"
"$litestream" sync -socket "$sock" -wait -timeout 30 "$db"
"$litestream" ltx -config "$cfg" -level all "$db" | sed 's/^/ltx: /'
"$litestream" restore -config "$cfg" -dry-run -o "$state/plan.sqlite3" "$db"

step "write the second batch, checkpoint from a second connection, then replicate"
write_batch_two
printf 'wal before checkpoint: %s bytes\n' "$(stat -c %s "$db-wal")"
printf 'PRAGMA wal_checkpoint(TRUNCATE): '
"$sqlite" -bail -cmd '.timeout 5000' "$db" 'PRAGMA wal_checkpoint(TRUNCATE);'
printf 'wal after checkpoint: %s bytes\n' "$(stat -c %s "$db-wal")"
"$litestream" sync -socket "$sock" -wait -timeout 30 "$db"
counts "$db" | sed 's/^/live: /'
"$litestream" ltx -config "$cfg" -level all "$db" | sed 's/^/ltx: /'

step "write the third batch after the checkpoint and replicate it"
write_batch_three
"$litestream" sync -socket "$sock" -wait -timeout 30 "$db"
counts "$db" | sed 's/^/live: /'
"$litestream" ltx -config "$cfg" -level all "$db" | sed 's/^/ltx: /'

expected=$(counts "$db")
printf 'expected rows: %s\n' "$expected"

step "stop replication and simulate loss of the live database"
kill -TERM "$litestream_pid"
wait "$litestream_pid" || true
litestream_pid=
for suffix in '' '-wal' '-shm'; do
  if [[ -e $db$suffix ]]; then mv "$db$suffix" "$db.lost$suffix"; fi
done
[[ ! -e $db ]] || die "live database still present"
printf 'moved %s and its sidecars to *.lost; replica retained at %s\n' "$db" "$replica"
du -sh "$replica" | sed 's/^/replica size: /'

step "restore to a scratch file and verify"
"$litestream" restore -config "$cfg" -json -integrity-check full -o "$restored" "$db"
"$sqlite" -bail -readonly "$restored" 'PRAGMA integrity_check;' | sed 's/^/integrity_check: /'
foreign=$("$sqlite" -bail -readonly "$restored" 'PRAGMA foreign_key_check;')
[[ -z $foreign ]] || die "foreign_key_check reported rows: $foreign"
printf 'foreign_key_check: ok (no rows)\n'
actual=$(counts "$restored")
printf 'restored rows: %s\n' "$actual"
[[ $actual == "$expected" ]] || die "restored counts differ: expected [$expected], got [$actual]"

step "serve the restored database on 127.0.0.1:$port"
"$server" >"$state/server.log" 2>&1 &
server_pid=$!
ready=
for _ in $(seq 1 100); do
  if ready=$(curl -fsS "http://127.0.0.1:$port/readyz" 2>/dev/null); then break; fi
  kill -0 "$server_pid" 2>/dev/null || die "server exited during startup; see $state/server.log"
  sleep 0.1
done
[[ -n $ready ]] || die "/readyz did not answer; see $state/server.log"
printf '/readyz: %s\n' "$ready"
[[ $ready == *'"status":"ok"'* ]] || die "readyz is not ok: $ready"
[[ $ready == *'"database":"ok"'* ]] || die "database is not ok: $ready"
printf '/healthz: %s\n' "$(curl -fsS "http://127.0.0.1:$port/healthz")"
kill -TERM "$server_pid"
wait "$server_pid" || true
server_pid=

step "result"
printf 'replicate: ok\nrestore: ok\nintegrity_check: ok\nrow counts: %s\nreadyz: ok\n' "$actual"
