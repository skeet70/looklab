-- A repeated payload completes the send that authorized it; a withdrawn latest
-- occurrence tombstones the group instead of reviving an older claim.

ALTER TABLE send_manifests ADD COLUMN withdrawn_at INTEGER;

CREATE TABLE manual_confirmation_occurrences (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    confirmation_id INTEGER NOT NULL REFERENCES manual_confirmations (id),
    send_nonce      TEXT NOT NULL,
    received_at     INTEGER NOT NULL,
    withdrawn_at    INTEGER
);

CREATE INDEX manual_confirmation_occurrences_confirmation
    ON manual_confirmation_occurrences (confirmation_id, id);
CREATE INDEX manual_confirmation_occurrences_send
    ON manual_confirmation_occurrences (send_nonce, withdrawn_at);
