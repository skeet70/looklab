-- The active community snapshot, independent of retriable proposal publication bookkeeping.

CREATE TABLE community_snapshot_activation (
    id           INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    version      INTEGER NOT NULL CHECK (version > 0),
    content_hash TEXT NOT NULL,
    input_version INTEGER,
    input_hash   TEXT,
    activated_at INTEGER NOT NULL
);

INSERT INTO community_snapshot_activation
    (id, version, content_hash, input_version, input_hash, activated_at)
SELECT 1,
       published_version,
       published_hash,
       NULL,
       NULL,
       CAST(strftime('%s', 'now') AS INTEGER)
FROM proposals
WHERE publication = 'published'
  AND published_version IS NOT NULL
  AND published_hash IS NOT NULL
ORDER BY published_version DESC
LIMIT 1;
