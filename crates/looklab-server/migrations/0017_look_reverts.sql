-- A moderator revert publishes a target version's content as a new version and
-- excludes credentials from every later fit of that Look (docs/05 §8).

CREATE TABLE look_credential_exclusions (
    look             TEXT NOT NULL,
    credential_id    TEXT NOT NULL REFERENCES credentials (id),
    target_version   INTEGER NOT NULL,
    reverted_version INTEGER NOT NULL,
    reason           TEXT NOT NULL,
    excluded_at      INTEGER NOT NULL,
    PRIMARY KEY (look, credential_id)
);

CREATE INDEX look_credential_exclusions_by_credential
    ON look_credential_exclusions (credential_id);
