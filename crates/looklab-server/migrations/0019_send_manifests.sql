-- One passkey approval per send (docs/03 §2.3, docs/05 §2): the signed send
-- manifest lists every batch hash. The nonce is the send instance; the manifest
-- hash is the signed binding and is not globally unique.

CREATE TABLE send_manifests (
    nonce           TEXT NOT NULL PRIMARY KEY,
    manifest_hash   BLOB NOT NULL,
    credential_id   BLOB NOT NULL,
    assertion_hash  BLOB NOT NULL,
    batch_count     INTEGER NOT NULL,
    received_count  INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    completed_at    INTEGER
);

CREATE INDEX send_manifests_by_manifest ON send_manifests (manifest_hash);

CREATE TABLE send_manifest_batches (
    send_nonce          TEXT NOT NULL REFERENCES send_manifests (nonce),
    batch_manifest_hash BLOB NOT NULL,
    received_at         INTEGER,
    contribution_id     TEXT REFERENCES contributions (id),
    PRIMARY KEY (send_nonce, batch_manifest_hash)
);

ALTER TABLE contributions ADD COLUMN send_nonce TEXT;

CREATE INDEX contributions_by_send ON contributions (send_nonce);
