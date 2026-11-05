-- Worker retry bookkeeping: released references and applied reputation are keyed
-- so a retried job resumes instead of repeating them (docs/02 §4.3).

CREATE TABLE withdrawal_releases (
    withdrawal_id TEXT NOT NULL REFERENCES withdrawals (id),
    image_id      TEXT NOT NULL REFERENCES contribution_images (id),
    released_at   INTEGER NOT NULL,
    PRIMARY KEY (withdrawal_id, image_id)
);

CREATE TABLE release_reputation_applied (
    look          TEXT NOT NULL,
    version       INTEGER NOT NULL,
    credential_id TEXT NOT NULL REFERENCES credentials (id),
    applied_at    INTEGER NOT NULL,
    PRIMARY KEY (look, version, credential_id)
);

CREATE TABLE look_pool_state (
    look        TEXT NOT NULL,
    body        TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (look, body)
);

CREATE TABLE release_body_params (
    look       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    body       TEXT NOT NULL,
    stage      TEXT NOT NULL CHECK (stage IN ('native', 'recipe')),
    params     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (look, version, body, stage)
);
