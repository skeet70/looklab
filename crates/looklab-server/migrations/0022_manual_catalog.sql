CREATE TABLE send_manifest_attestations (
    send_nonce       TEXT NOT NULL REFERENCES send_manifests (nonce),
    attestation_hash BLOB NOT NULL,
    received_at      INTEGER,
    confirmation_id  INTEGER,
    PRIMARY KEY (send_nonce, attestation_hash)
);

CREATE TABLE manual_confirmations (
    id             INTEGER PRIMARY KEY AUTOINCREMENT,
    credential_id  TEXT NOT NULL,
    camera_id      TEXT NOT NULL,
    camera_digest  BLOB NOT NULL,
    firmware       TEXT NOT NULL,
    scope_kind     TEXT NOT NULL,
    scope_json     TEXT NOT NULL,
    payload_digest BLOB NOT NULL,
    assertion_hash BLOB NOT NULL,
    send_nonce     TEXT,
    created_at     INTEGER NOT NULL
);

CREATE UNIQUE INDEX manual_confirmations_payload
    ON manual_confirmations (credential_id, payload_digest);
CREATE INDEX manual_confirmations_camera
    ON manual_confirmations (camera_id, created_at);

CREATE TABLE manual_observations (
    confirmation_id   INTEGER NOT NULL REFERENCES manual_confirmations (id),
    capability_key    TEXT NOT NULL,
    definition_digest BLOB NOT NULL,
    finding_json      TEXT NOT NULL,
    PRIMARY KEY (confirmation_id, capability_key)
);

CREATE TABLE manual_moderator_confirmations (
    id                  INTEGER PRIMARY KEY AUTOINCREMENT,
    camera_id           TEXT NOT NULL,
    capability_key      TEXT NOT NULL,
    definition_digest   BLOB NOT NULL,
    firmware_scope      TEXT NOT NULL,
    moderator_credential TEXT NOT NULL,
    confirmed           INTEGER NOT NULL,
    evidence_kind       TEXT NOT NULL,
    evidence_url        TEXT,
    evidence_description TEXT NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE INDEX manual_moderator_current
    ON manual_moderator_confirmations (camera_id, capability_key, created_at);
