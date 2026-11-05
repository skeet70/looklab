-- Recipe outcomes, scores, persistent residuals and published bounties
-- (docs/03 §1, docs/05 §12). Predicted ΔE is recorded by the verification path;
-- the worker recomputes scores, residuals and bounties from these rows.

CREATE TABLE recipe_observations (
    id                       TEXT NOT NULL PRIMARY KEY,
    contribution_id          TEXT NOT NULL REFERENCES contributions (id),
    image_id                 TEXT NOT NULL REFERENCES contribution_images (id),
    body_id                  TEXT NOT NULL,
    epoch                    INTEGER NOT NULL,
    base_sim                 TEXT NOT NULL,
    settings                 TEXT NOT NULL,
    target_look              TEXT NOT NULL,
    region                   TEXT NOT NULL,
    contributor_id           TEXT NOT NULL,
    tier                     TEXT NOT NULL CHECK (tier IN ('new', 'endorsed', 'seeder')),
    root                     INTEGER NOT NULL DEFAULT 0 CHECK (root IN (0, 1)),
    residual_de              REAL NOT NULL,
    sigma                    REAL NOT NULL CHECK (sigma > 0),
    predicted_de             REAL,
    predicted_sigma          REAL,
    measured_de              REAL,
    input_transform_version  INTEGER NOT NULL DEFAULT 0,
    control_response_version INTEGER NOT NULL DEFAULT 0,
    look_version             INTEGER NOT NULL DEFAULT 0,
    created_at               INTEGER NOT NULL,
    UNIQUE (image_id, region)
);

CREATE INDEX recipe_observations_by_target
    ON recipe_observations (target_look, body_id, region);

CREATE TABLE recipe_scores (
    id                       TEXT NOT NULL PRIMARY KEY,
    body_id                  TEXT NOT NULL,
    epoch                    INTEGER NOT NULL,
    base_sim                 TEXT NOT NULL,
    settings                 TEXT NOT NULL,
    target_look              TEXT NOT NULL,
    predicted_de             REAL NOT NULL,
    predicted_sigma          REAL NOT NULL,
    measured_de              REAL,
    measured_sigma           REAL,
    n_verifications          INTEGER NOT NULL,
    input_transform_version  INTEGER NOT NULL,
    control_response_version INTEGER NOT NULL,
    look_version             INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    UNIQUE (body_id, epoch, base_sim, settings, target_look)
);

CREATE INDEX recipe_scores_by_look_body ON recipe_scores (target_look, body_id);

CREATE TABLE recipe_residuals (
    id               TEXT NOT NULL PRIMARY KEY,
    target_look      TEXT NOT NULL,
    body_id          TEXT NOT NULL,
    region           TEXT NOT NULL,
    contributors     INTEGER NOT NULL,
    mean_residual    REAL NOT NULL,
    sigma            REAL NOT NULL,
    reachable        INTEGER NOT NULL CHECK (reachable IN (0, 1)),
    persistent       INTEGER NOT NULL CHECK (persistent IN (0, 1)),
    attribution      TEXT NOT NULL,
    alignment        TEXT NOT NULL,
    updated_at       INTEGER NOT NULL,
    UNIQUE (target_look, body_id, region)
);

CREATE TABLE bounties (
    id                          TEXT NOT NULL PRIMARY KEY,
    target_look                 TEXT NOT NULL,
    look_version                INTEGER NOT NULL,
    body_id                     TEXT,
    region                      TEXT NOT NULL,
    expected_variance_reduction REAL NOT NULL,
    reason                      TEXT NOT NULL,
    status                      TEXT NOT NULL DEFAULT 'open'
                                     CHECK (status IN ('open', 'resolved')),
    created_at                  INTEGER NOT NULL,
    resolved_at                 INTEGER
);

CREATE INDEX bounties_by_rank
    ON bounties (status, expected_variance_reduction DESC);
