-- Deferred monochrome intake (local://bw-intake-decision.md): signed render
-- class, snapshot schema and per-image metadata classification, and expiry.

ALTER TABLE contributions ADD COLUMN render_class TEXT NOT NULL DEFAULT 'colour'
    CHECK (render_class IN ('colour', 'monochrome'));
ALTER TABLE contributions ADD COLUMN monochrome_authorization TEXT
    CHECK (monochrome_authorization IS NULL
           OR monochrome_authorization = 'model_development_180_days');
ALTER TABLE contributions ADD COLUMN deferred_until INTEGER;
ALTER TABLE contributions ADD COLUMN community_snapshot_version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE contributions ADD COLUMN normalized_settings TEXT NOT NULL DEFAULT '[]';

CREATE INDEX contributions_deferred ON contributions (render_class, deferred_until);

ALTER TABLE contribution_images ADD COLUMN render_class TEXT NOT NULL DEFAULT 'colour'
    CHECK (render_class IN ('colour', 'monochrome'));
ALTER TABLE contribution_images ADD COLUMN native_sim TEXT;
ALTER TABLE contribution_images ADD COLUMN family TEXT;
ALTER TABLE contribution_images ADD COLUMN mono_filter TEXT
    CHECK (mono_filter IS NULL OR mono_filter IN ('none', 'yellow', 'red', 'green'));
ALTER TABLE contribution_images ADD COLUMN blob_released_at INTEGER;

UPDATE contribution_images
SET blob_released_at = (
    SELECT MAX(wr.released_at) FROM withdrawal_releases wr WHERE wr.image_id = contribution_images.id
)
WHERE EXISTS (SELECT 1 FROM withdrawal_releases wr WHERE wr.image_id = contribution_images.id);

CREATE TABLE deferred_expiries (
    id              TEXT NOT NULL PRIMARY KEY,
    contribution_id TEXT NOT NULL UNIQUE REFERENCES contributions (id),
    due_at          INTEGER NOT NULL,
    status          TEXT NOT NULL DEFAULT 'pending'
                            CHECK (status IN ('pending', 'deleted'))
);

CREATE INDEX deferred_expiries_due ON deferred_expiries (status, due_at);
