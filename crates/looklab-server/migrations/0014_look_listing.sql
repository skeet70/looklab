CREATE TABLE look_listings (
    look          TEXT NOT NULL PRIMARY KEY,
    listing       TEXT NOT NULL DEFAULT 'listed'
                         CHECK (listing IN ('listed', 'hidden', 'removed')),
    superseded_by TEXT,
    updated_at    INTEGER NOT NULL
);
