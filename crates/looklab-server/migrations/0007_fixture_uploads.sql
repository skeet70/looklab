-- Fixture files a credential uploaded, private until a moderator approves a
-- proposal that cites them. The blob itself lives content-addressed on disk.

CREATE TABLE fixture_uploads (
    sha256        BLOB NOT NULL,
    credential_id TEXT NOT NULL REFERENCES credentials (id),
    file          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    PRIMARY KEY (sha256, credential_id)
);

CREATE INDEX fixture_uploads_by_credential ON fixture_uploads (credential_id, created_at);
