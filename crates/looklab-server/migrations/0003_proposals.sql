-- Interface profiles carry a round-trip report; discussion is proposer + moderators.

ALTER TABLE proposals ADD COLUMN round_trip_report TEXT;

CREATE TABLE proposal_messages (
    id              TEXT NOT NULL PRIMARY KEY,
    proposal_id     TEXT NOT NULL REFERENCES proposals (id),
    author          TEXT NOT NULL CHECK (author IN ('proposer', 'moderator')),
    author_credential TEXT NOT NULL REFERENCES credentials (id),
    body            TEXT NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE INDEX proposal_messages_by_proposal ON proposal_messages (proposal_id, created_at);
