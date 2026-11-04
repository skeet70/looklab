-- An approval records the snapshot version it publishes before the rename.

ALTER TABLE proposals ADD COLUMN publication TEXT
    CHECK (publication IN ('pending', 'published'));
ALTER TABLE proposals ADD COLUMN published_version INTEGER;
ALTER TABLE proposals ADD COLUMN published_hash TEXT;
