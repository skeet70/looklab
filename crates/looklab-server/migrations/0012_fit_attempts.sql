-- Non-converged native fit state and moderation holds ([docs/02 §4.3]).

CREATE TABLE fit_attempts (
    look        TEXT NOT NULL PRIMARY KEY,
    states      TEXT NOT NULL,
    consecutive INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE fit_holds (
    look          TEXT NOT NULL PRIMARY KEY,
    consecutive   INTEGER NOT NULL,
    first_attempt INTEGER NOT NULL,
    last_attempt  INTEGER NOT NULL,
    diagnostics   TEXT NOT NULL,
    status        TEXT NOT NULL
);
