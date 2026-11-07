-- Durable look-scoped refit requests from a moderator revert (docs/05 §8).
-- The generation lets a worker clear only the request it processed.

CREATE TABLE look_refit_requests (
    look             TEXT NOT NULL PRIMARY KEY,
    generation       INTEGER NOT NULL,
    reverted_version INTEGER NOT NULL,
    requested_at     INTEGER NOT NULL
);
