-- A publication request outlives the publisher already running when it arrives.
-- `generation` counts requests; `published` reached disk; `attempted` failed terminally.

CREATE TABLE publication (
    id         INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL DEFAULT 0,
    published  INTEGER NOT NULL DEFAULT 0,
    attempted  INTEGER NOT NULL DEFAULT 0
);

INSERT INTO publication (id, generation, published, attempted) VALUES (1, 0, 0, 0);
