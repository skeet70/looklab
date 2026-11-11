-- Durable per-subject pending-work consumption (docs/05 §8.3).
CREATE TABLE model_pending_consumption (
    subject_key  TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    generation   INTEGER NOT NULL CHECK (generation >= 0),
    consumed_at  INTEGER NOT NULL
);
