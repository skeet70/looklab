-- Versioned post-clip recipe evidence (docs/02 §5, docs/03, docs/05 §12).
-- Existing rows keep their pre-clip meaning under the legacy metric identity.

CREATE TABLE recipe_observations_new (
    id                       TEXT NOT NULL PRIMARY KEY,
    contribution_id          TEXT NOT NULL REFERENCES contributions (id),
    image_id                 TEXT NOT NULL REFERENCES contribution_images (id),
    body_id                  TEXT NOT NULL,
    epoch                    INTEGER NOT NULL,
    base_sim                 TEXT NOT NULL,
    settings                 TEXT NOT NULL,
    target_look              TEXT NOT NULL,
    region                   TEXT NOT NULL,
    metric                   TEXT NOT NULL DEFAULT 'looklab-scoring-metric/legacy-pre-clip/1',
    target_encoding          TEXT,
    target_clip              REAL,
    output_encoding          TEXT,
    realization_clip         REAL,
    contributor_id           TEXT NOT NULL,
    tier                     TEXT NOT NULL CHECK (tier IN ('new', 'endorsed', 'seeder')),
    root                     INTEGER NOT NULL DEFAULT 0 CHECK (root IN (0, 1)),
    residual_de              REAL NOT NULL,
    raw_gap                  REAL,
    sigma                    REAL NOT NULL CHECK (sigma > 0),
    observation_sigma        REAL,
    measured_de              REAL,
    predicted_de             REAL,
    predicted_sigma          REAL,
    signed_residual          TEXT,
    prediction_residual      TEXT,
    assessment               TEXT NOT NULL DEFAULT 'unassessed',
    systematic_sigma         REAL,
    input_transform_version  INTEGER NOT NULL DEFAULT 0,
    control_response_version INTEGER NOT NULL DEFAULT 0,
    look_version             INTEGER NOT NULL DEFAULT 0,
    source_realization       TEXT,
    source_body_state        INTEGER,
    source_report            TEXT,
    source_scope             TEXT,
    created_at               INTEGER NOT NULL,
    UNIQUE (image_id, region, metric)
);

INSERT INTO recipe_observations_new (
    id, contribution_id, image_id, body_id, epoch, base_sim, settings, target_look,
    region, metric, target_encoding, target_clip, output_encoding, realization_clip,
    contributor_id, tier, root, residual_de, raw_gap, sigma, observation_sigma,
    measured_de, predicted_de, predicted_sigma, signed_residual, prediction_residual,
    assessment, systematic_sigma, input_transform_version, control_response_version,
    look_version, source_realization, source_body_state, source_report, source_scope,
    created_at
)
SELECT id, contribution_id, image_id, body_id, epoch, base_sim, settings, target_look,
       region, 'looklab-scoring-metric/legacy-pre-clip/1', NULL, NULL, NULL, NULL,
       contributor_id, tier, root, residual_de, residual_de, sigma, sigma, measured_de,
       predicted_de, predicted_sigma, NULL, NULL, 'unassessed', NULL,
       input_transform_version, control_response_version, look_version,
       source_realization, source_body_state, source_report, source_scope, created_at
FROM recipe_observations;

DROP TABLE recipe_observations;
ALTER TABLE recipe_observations_new RENAME TO recipe_observations;
CREATE INDEX recipe_observations_by_target
    ON recipe_observations (target_look, body_id, region);

CREATE TABLE recipe_scores_new (
    id                       TEXT NOT NULL PRIMARY KEY,
    body_id                  TEXT NOT NULL,
    epoch                    INTEGER NOT NULL,
    base_sim                 TEXT NOT NULL,
    settings                 TEXT NOT NULL,
    target_look              TEXT NOT NULL,
    metric                   TEXT NOT NULL DEFAULT 'looklab-scoring-metric/legacy-pre-clip/1',
    predicted_de             REAL NOT NULL,
    predicted_sigma          REAL,
    measured_de              REAL,
    measured_sigma           REAL,
    raw_gap                  REAL,
    signed_residual          TEXT,
    n_verifications          INTEGER NOT NULL,
    input_transform_version  INTEGER NOT NULL,
    control_response_version INTEGER NOT NULL,
    look_version             INTEGER NOT NULL,
    target_encoding          TEXT,
    target_clip              REAL,
    output_encoding          TEXT,
    realization_clip         REAL,
    source_realization       TEXT,
    source_body_state        INTEGER,
    source_report            TEXT,
    source_scope             TEXT,
    updated_at               INTEGER NOT NULL,
    UNIQUE (body_id, epoch, base_sim, settings, target_look, source_realization, source_body_state, source_report, source_scope, metric, target_encoding, target_clip, output_encoding, realization_clip)
);

INSERT INTO recipe_scores_new (
    id, body_id, epoch, base_sim, settings, target_look, metric, predicted_de,
    predicted_sigma, measured_de, measured_sigma, raw_gap, signed_residual,
    n_verifications, input_transform_version, control_response_version, look_version,
    target_encoding, target_clip, output_encoding, realization_clip,
    source_realization, source_body_state, source_report, source_scope, updated_at
)
SELECT id, body_id, epoch, base_sim, settings, target_look, 'looklab-scoring-metric/legacy-pre-clip/1',
       predicted_de, predicted_sigma, measured_de, measured_sigma,
       CASE WHEN measured_de IS NULL THEN NULL ELSE measured_de - predicted_de END,
       NULL, n_verifications, input_transform_version, control_response_version,
       look_version, NULL, NULL, NULL, NULL, source_realization, source_body_state,
       source_report, source_scope, updated_at
FROM recipe_scores;

DROP TABLE recipe_scores;
ALTER TABLE recipe_scores_new RENAME TO recipe_scores;
CREATE INDEX recipe_scores_by_look_body ON recipe_scores (target_look, body_id);

CREATE TABLE recipe_residuals_new (
    id               TEXT NOT NULL PRIMARY KEY,
    target_look      TEXT NOT NULL,
    body_id          TEXT NOT NULL,
    region           TEXT NOT NULL,
    cell             TEXT NOT NULL,
    target_encoding  TEXT,
    target_clip      REAL,
    output_encoding  TEXT,
    realization_clip REAL,
    contributors     INTEGER NOT NULL,
    settings_cells   INTEGER NOT NULL DEFAULT 1,
    mean_residual    REAL NOT NULL,
    raw_gap          REAL,
    observation_sigma REAL,
    sigma            REAL NOT NULL,
    systematic_excess REAL,
    systematic_sigma REAL,
    assessment       TEXT NOT NULL DEFAULT 'unassessed',
    reachable        INTEGER CHECK (reachable IN (0, 1)),
    persistent       INTEGER NOT NULL CHECK (persistent IN (0, 1)),
    attribution      TEXT NOT NULL,
    alignment        TEXT NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (target_look, body_id, region, target_encoding, target_clip, output_encoding, realization_clip)
);

INSERT INTO recipe_residuals_new (
    id, target_look, body_id, region, cell, target_encoding, target_clip,
    output_encoding, realization_clip, contributors, settings_cells, mean_residual,
    raw_gap, observation_sigma, sigma, systematic_excess, systematic_sigma,
    assessment, reachable, persistent, attribution, alignment, updated_at
)
SELECT id, target_look, body_id, region, region, NULL, NULL, NULL, NULL,
       contributors, 1, mean_residual, mean_residual, NULL, sigma, NULL, NULL,
       'unassessed', reachable, persistent, attribution, alignment, updated_at
FROM recipe_residuals;

DROP TABLE recipe_residuals;
ALTER TABLE recipe_residuals_new RENAME TO recipe_residuals;

ALTER TABLE bounties ADD COLUMN metric TEXT NOT NULL DEFAULT 'looklab-scoring-metric/legacy-pre-clip/1';
ALTER TABLE bounties ADD COLUMN target_encoding TEXT;
ALTER TABLE bounties ADD COLUMN target_clip REAL;
ALTER TABLE bounties ADD COLUMN output_encoding TEXT;
ALTER TABLE bounties ADD COLUMN realization_clip REAL;
