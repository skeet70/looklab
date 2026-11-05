-- Worker proposals have no credential; SQLite cannot drop NOT NULL in place, so
-- the table and both children are rebuilt.

CREATE TABLE proposals_new (
    id                TEXT NOT NULL PRIMARY KEY,
    credential_id     TEXT REFERENCES credentials (id),
    origin            TEXT NOT NULL DEFAULT 'credential'
                             CHECK (origin IN ('credential', 'worker')),
    type              TEXT NOT NULL
                            CHECK (type IN ('body', 'schema_change', 'mapping', 'interface_profile',
                                            'lut_look', 'epoch_split', 'look_split', 'gate_override')),
    payload           TEXT NOT NULL,
    human_summary     TEXT NOT NULL,
    evidence          TEXT NOT NULL DEFAULT '[]',
    status            TEXT NOT NULL,
    created_at        INTEGER NOT NULL,
    round_trip_report TEXT,
    publication       TEXT CHECK (publication IN ('pending', 'published')),
    published_version INTEGER,
    published_hash    TEXT
);

INSERT INTO proposals_new
    (id, credential_id, origin, type, payload, human_summary, evidence, status, created_at,
     round_trip_report, publication, published_version, published_hash)
SELECT
    id, credential_id, 'credential', type, payload, human_summary, evidence, status, created_at,
    round_trip_report, publication, published_version, published_hash
FROM proposals;

CREATE TABLE proposal_messages_new (
    id                TEXT NOT NULL PRIMARY KEY,
    proposal_id       TEXT NOT NULL REFERENCES proposals_new (id),
    author            TEXT NOT NULL CHECK (author IN ('proposer', 'moderator')),
    author_credential TEXT NOT NULL REFERENCES credentials (id),
    body              TEXT NOT NULL,
    created_at        INTEGER NOT NULL
);

INSERT INTO proposal_messages_new
    (id, proposal_id, author, author_credential, body, created_at)
SELECT id, proposal_id, author, author_credential, body, created_at
FROM proposal_messages;

CREATE TABLE moderation_decisions_new (
    id                   TEXT NOT NULL PRIMARY KEY,
    proposal_id          TEXT NOT NULL REFERENCES proposals_new (id),
    moderator_credential TEXT NOT NULL REFERENCES credentials (id),
    decision             TEXT NOT NULL,
    reason               TEXT NOT NULL,
    decided_at           INTEGER NOT NULL
);

INSERT INTO moderation_decisions_new
    (id, proposal_id, moderator_credential, decision, reason, decided_at)
SELECT id, proposal_id, moderator_credential, decision, reason, decided_at
FROM moderation_decisions;

DROP TABLE moderation_decisions;
ALTER TABLE moderation_decisions_new RENAME TO moderation_decisions;

DROP TABLE proposal_messages;
ALTER TABLE proposal_messages_new RENAME TO proposal_messages;

DROP TABLE proposals;
ALTER TABLE proposals_new RENAME TO proposals;

CREATE INDEX proposals_by_credential ON proposals (credential_id, created_at);
CREATE INDEX proposals_by_status ON proposals (status, created_at);
CREATE INDEX proposal_messages_by_proposal ON proposal_messages (proposal_id, created_at);
CREATE INDEX moderation_decisions_by_proposal ON moderation_decisions (proposal_id, decided_at);
