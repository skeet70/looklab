-- Custom-look LUT files carried with a lut_look proposal.

CREATE TABLE lut_uploads (
    proposal_id   TEXT NOT NULL PRIMARY KEY REFERENCES proposals (id),
    sha256        BLOB NOT NULL,
    credential_id TEXT NOT NULL REFERENCES credentials (id),
    file          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE INDEX lut_uploads_by_credential ON lut_uploads (credential_id, created_at);
CREATE INDEX lut_uploads_by_hash ON lut_uploads (sha256);

DROP VIEW blob_references;
CREATE VIEW blob_references AS
    SELECT lower(hex(hash)) AS hash FROM blobs
    UNION
    SELECT lower(hex(hash)) FROM blob_staging
    UNION
    SELECT lower(hex(sha256)) FROM fixture_uploads
    UNION
    SELECT lower(hex(sha256)) FROM lut_uploads
    UNION
    SELECT lower(json_extract(artifact.value, '$.sha256'))
      FROM version_manifests, json_each(version_manifests.artifacts) AS artifact;

-- Idempotency record for a retried import job.
CREATE TABLE lut_imports (
    proposal_id TEXT PRIMARY KEY REFERENCES proposals (id),
    look        TEXT NOT NULL,
    version     INTEGER NOT NULL,
    created_at  INTEGER NOT NULL
);

-- The permanent reason an approved LUT proposal produced no version.
CREATE TABLE lut_import_failures (
    proposal_id TEXT PRIMARY KEY REFERENCES proposals (id),
    reason      TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);

-- Blobs whose LUT reference was released; the sweep retries until unlink succeeds.
CREATE TABLE lut_blob_deletions (
    hash      BLOB NOT NULL PRIMARY KEY,
    queued_at INTEGER NOT NULL
);
