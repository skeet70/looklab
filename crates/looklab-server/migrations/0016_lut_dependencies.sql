-- Display-base pins of published lut_defined versions and their rebase work (docs/05 §8).

CREATE TABLE lut_base_pins (
    look         TEXT NOT NULL,
    version      INTEGER NOT NULL,
    base         TEXT NOT NULL,
    base_status  TEXT NOT NULL,
    base_look    TEXT,
    base_version INTEGER,
    recorded_at  INTEGER NOT NULL,
    PRIMARY KEY (look, version)
);

CREATE INDEX lut_base_pins_by_base ON lut_base_pins (base_look, base_version);

CREATE TABLE lut_native_renderings (
    look         TEXT NOT NULL,
    version      INTEGER NOT NULL,
    maker        TEXT NOT NULL,
    rendering    TEXT NOT NULL,
    published_at INTEGER NOT NULL,
    PRIMARY KEY (look, version)
);

-- A published LUT version waiting for its next candidate.
CREATE TABLE lut_rebase_pending (
    look      TEXT NOT NULL,
    version   INTEGER NOT NULL,
    reason    TEXT NOT NULL
                     CHECK (reason IN ('base_published', 'base_revoked', 'family_resolved')),
    marked_at INTEGER NOT NULL,
    PRIMARY KEY (look, version)
);
