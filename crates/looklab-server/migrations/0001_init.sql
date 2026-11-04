-- Entities from docs/03 §1. Ids are UUID text; JSON payloads are validated in
-- the application before they reach a column.

CREATE TABLE credentials (
    id                  TEXT NOT NULL PRIMARY KEY,
    credential_id       BLOB NOT NULL UNIQUE,
    webauthn_credential TEXT NOT NULL,
    display_handle      TEXT,
    tier                TEXT NOT NULL DEFAULT 'new'
                                CHECK (tier IN ('new', 'endorsed', 'seeder')),
    roles               TEXT NOT NULL DEFAULT '[]',
    reputation          REAL NOT NULL DEFAULT 0.5,
    created_at          INTEGER NOT NULL,
    status              TEXT NOT NULL DEFAULT 'active'
                                CHECK (status IN ('active', 'revoked'))
);

CREATE TABLE grants (
    id                TEXT NOT NULL PRIMARY KEY,
    subject_credential TEXT NOT NULL REFERENCES credentials (id),
    role_or_tier      TEXT NOT NULL CHECK (role_or_tier IN ('seeder', 'moderator', 'admin')),
    scope             TEXT,
    granted_by        TEXT NOT NULL,
    reason            TEXT NOT NULL,
    granted_at        INTEGER NOT NULL,
    revoked_at        INTEGER
);

CREATE TABLE endorsements (
    id                 TEXT NOT NULL PRIMARY KEY,
    issuer_credential  TEXT NOT NULL REFERENCES credentials (id),
    subject_credential TEXT NOT NULL REFERENCES credentials (id),
    scope              TEXT NOT NULL,
    reason             TEXT NOT NULL,
    evidence_refs      TEXT NOT NULL DEFAULT '[]',
    issued_at          INTEGER NOT NULL,
    revoked_at         INTEGER
);

CREATE TABLE root_admissions (
    id              TEXT NOT NULL PRIMARY KEY,
    admitted_by     TEXT NOT NULL REFERENCES credentials (id),
    contribution_id TEXT NOT NULL,
    scope           TEXT NOT NULL,
    reason          TEXT NOT NULL,
    admitted_at     INTEGER NOT NULL,
    revoked_at      INTEGER
);

CREATE TABLE contributions (
    id                  TEXT NOT NULL PRIMARY KEY,
    credential_id       TEXT NOT NULL REFERENCES credentials (id),
    batch_manifest_hash BLOB NOT NULL,
    signature           BLOB NOT NULL,
    pow                 TEXT NOT NULL,
    kind                TEXT NOT NULL
                                CHECK (kind IN ('library', 'rerender', 'pattern', 'gradient', 'chart')),
    serial_hmac         BLOB,
    intent              TEXT NOT NULL,
    target_look_id      TEXT,
    body_id             TEXT NOT NULL,
    firmware_version    TEXT NOT NULL,
    sim_key             TEXT NOT NULL,
    settings            TEXT NOT NULL,
    status              TEXT NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'accepted', 'quarantined', 'withdrawn')),
    created_at          INTEGER NOT NULL
);

CREATE INDEX contributions_by_credential ON contributions (credential_id, created_at);

CREATE TABLE contribution_images (
    id              TEXT NOT NULL PRIMARY KEY,
    contribution_id TEXT NOT NULL REFERENCES contributions (id),
    samples_hash    BLOB NOT NULL,
    n_samples       INTEGER NOT NULL,
    meta            TEXT NOT NULL,
    rerender_group  TEXT,
    duplicate_of    TEXT,
    rms_de          REAL,
    z               REAL,
    reason          TEXT,
    weight_components TEXT,
    status          TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'accepted', 'down_weighted', 'quarantined')),
    status_reason   TEXT
);

CREATE INDEX contribution_images_by_contribution ON contribution_images (contribution_id);

CREATE TABLE blobs (
    hash      BLOB NOT NULL PRIMARY KEY,
    ref_count INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE proposals (
    id             TEXT NOT NULL PRIMARY KEY,
    credential_id  TEXT NOT NULL REFERENCES credentials (id),
    type           TEXT NOT NULL
                           CHECK (type IN ('body', 'schema_change', 'mapping', 'interface_profile',
                                           'lut_look', 'epoch_split', 'look_split', 'gate_override')),
    payload        TEXT NOT NULL,
    human_summary  TEXT NOT NULL,
    evidence       TEXT NOT NULL DEFAULT '[]',
    status         TEXT NOT NULL,
    created_at     INTEGER NOT NULL
);

CREATE TABLE moderation_decisions (
    id                   TEXT NOT NULL PRIMARY KEY,
    proposal_id          TEXT NOT NULL REFERENCES proposals (id),
    moderator_credential TEXT NOT NULL REFERENCES credentials (id),
    decision             TEXT NOT NULL,
    reason               TEXT NOT NULL,
    decided_at           INTEGER NOT NULL
);

CREATE TABLE audit_log (
    seq     INTEGER PRIMARY KEY AUTOINCREMENT,
    type    TEXT NOT NULL,
    payload TEXT NOT NULL,
    ts      INTEGER NOT NULL
);

CREATE TRIGGER audit_log_no_replace
BEFORE INSERT ON audit_log
WHEN NEW.seq > 0
 AND NEW.seq <= (SELECT COALESCE(MAX(seq), 0) FROM audit_log)
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TRIGGER audit_log_no_update
BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TRIGGER audit_log_no_delete
BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TABLE jobs (
    id           TEXT NOT NULL PRIMARY KEY,
    kind         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    status       TEXT NOT NULL DEFAULT 'queued'
                         CHECK (status IN ('queued', 'running', 'done', 'failed')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    run_after    INTEGER NOT NULL,
    locked_until INTEGER,
    created_at   INTEGER NOT NULL
);

CREATE INDEX jobs_ready ON jobs (status, run_after);

CREATE TABLE status_tokens (
    token_hash    BLOB NOT NULL PRIMARY KEY,
    credential_id TEXT NOT NULL REFERENCES credentials (id),
    issued_at     INTEGER NOT NULL,
    revoked_at    INTEGER
);

CREATE UNIQUE INDEX status_tokens_one_active
    ON status_tokens (credential_id) WHERE revoked_at IS NULL;

CREATE TABLE nonces (
    nonce      TEXT NOT NULL PRIMARY KEY,
    issued_at  INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at    INTEGER
);

CREATE INDEX nonces_expiry ON nonces (expires_at);

CREATE TABLE withdrawals (
    id              TEXT NOT NULL PRIMARY KEY,
    contribution_id TEXT NOT NULL REFERENCES contributions (id),
    credential_id   TEXT NOT NULL REFERENCES credentials (id),
    requested_at    INTEGER NOT NULL,
    delete_after    INTEGER NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'deleted'))
);
