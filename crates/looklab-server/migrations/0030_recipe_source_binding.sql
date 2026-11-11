-- Exact immutable source binding and explicit unassessed evidence (docs/05 §12).

ALTER TABLE recipe_observations ADD COLUMN source_realization TEXT;
ALTER TABLE recipe_observations ADD COLUMN source_body_state INTEGER;
ALTER TABLE recipe_observations ADD COLUMN source_report TEXT;
ALTER TABLE recipe_observations ADD COLUMN source_scope TEXT;

CREATE TABLE recipe_scores_new (
    id                       TEXT NOT NULL PRIMARY KEY,
    body_id                  TEXT NOT NULL,
    epoch                    INTEGER NOT NULL,
    base_sim                 TEXT NOT NULL,
    settings                 TEXT NOT NULL,
    target_look              TEXT NOT NULL,
    predicted_de             REAL NOT NULL,
    predicted_sigma          REAL,
    measured_de              REAL,
    measured_sigma           REAL,
    n_verifications          INTEGER NOT NULL,
    input_transform_version  INTEGER NOT NULL,
    control_response_version INTEGER NOT NULL,
    look_version             INTEGER NOT NULL,
    source_realization       TEXT,
    source_body_state        INTEGER,
    source_report            TEXT,
    source_scope             TEXT,
    updated_at               INTEGER NOT NULL,
    UNIQUE (body_id, epoch, base_sim, settings, target_look, source_realization, source_body_state, source_report, source_scope)
);

INSERT INTO recipe_scores_new (
    id, body_id, epoch, base_sim, settings, target_look, predicted_de, predicted_sigma,
    measured_de, measured_sigma, n_verifications, input_transform_version,
    control_response_version, look_version, updated_at
)
SELECT id, body_id, epoch, base_sim, settings, target_look, predicted_de, predicted_sigma,
       measured_de, measured_sigma, n_verifications, input_transform_version,
       control_response_version, look_version, updated_at
FROM recipe_scores;

DROP TABLE recipe_scores;
ALTER TABLE recipe_scores_new RENAME TO recipe_scores;
CREATE INDEX recipe_scores_by_look_body ON recipe_scores (target_look, body_id);

UPDATE recipe_scores SET predicted_sigma = NULL WHERE source_realization IS NULL;

CREATE TABLE recipe_residuals_new (
    id               TEXT NOT NULL PRIMARY KEY,
    target_look      TEXT NOT NULL,
    body_id          TEXT NOT NULL,
    region           TEXT NOT NULL,
    contributors     INTEGER NOT NULL,
    mean_residual    REAL NOT NULL,
    sigma            REAL NOT NULL,
    reachable        INTEGER CHECK (reachable IN (0, 1)),
    persistent       INTEGER NOT NULL CHECK (persistent IN (0, 1)),
    attribution      TEXT NOT NULL,
    alignment        TEXT NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (target_look, body_id, region)
);

INSERT INTO recipe_residuals_new (
    id, target_look, body_id, region, contributors, mean_residual, sigma, reachable,
    persistent, attribution, alignment, updated_at
)
SELECT id, target_look, body_id, region, contributors, mean_residual, sigma, reachable,
       persistent, attribution, alignment, updated_at
FROM recipe_residuals;

DROP TABLE recipe_residuals;
ALTER TABLE recipe_residuals_new RENAME TO recipe_residuals;

UPDATE recipe_residuals
SET reachable = NULL, attribution = '"unassessed"', alignment = 'null';
