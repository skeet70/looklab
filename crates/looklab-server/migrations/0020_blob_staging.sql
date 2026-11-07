-- Committed before the blob file is written, so a crash or a rejected
-- transaction leaves a record that lets the worker remove the file.
CREATE TABLE blob_staging (
    attempt_id TEXT NOT NULL,
    hash       BLOB NOT NULL,
    staged_at  INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    PRIMARY KEY (attempt_id, hash)
);

CREATE INDEX blob_staging_by_hash ON blob_staging (hash);

-- A file is deletable only when its hash is absent here.
CREATE VIEW blob_references AS
    SELECT lower(hex(hash)) AS hash FROM blobs
    UNION
    SELECT lower(hex(hash)) FROM blob_staging
    UNION
    SELECT lower(hex(sha256)) FROM fixture_uploads
    UNION
    SELECT lower(json_extract(artifact.value, '$.sha256'))
      FROM version_manifests, json_each(version_manifests.artifacts) AS artifact;
