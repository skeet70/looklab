-- Scoped model qualification storage (docs/03 §1, docs/model-qualification-plan.md §8).
-- Legacy `release_body_params`/`look_pool_state` lack epoch and sim, so their scope stays unassessed.

CREATE TABLE model_subjects (
    key        TEXT NOT NULL PRIMARY KEY,
    kind       TEXT NOT NULL CHECK (kind IN ('shared_family', 'body_realization')),
    family     TEXT,
    body       TEXT,
    epoch      INTEGER,
    native_sim TEXT,
    created_at INTEGER NOT NULL,
    CHECK (
        (kind = 'shared_family'
            AND family IS NOT NULL AND body IS NULL AND epoch IS NULL AND native_sim IS NULL)
        OR
        (kind = 'body_realization'
            AND family IS NULL AND body IS NOT NULL AND epoch IS NOT NULL AND native_sim IS NOT NULL)
    )
);

CREATE TABLE model_legacy_scopes (
    kind          TEXT NOT NULL CHECK (kind IN ('look_version', 'body_params', 'pool_state')),
    target        TEXT NOT NULL,
    family        TEXT,
    body          TEXT,
    applicability TEXT NOT NULL CHECK (applicability = 'unassessed'),
    reason        TEXT NOT NULL CHECK (reason = 'epoch_and_sim_unrecorded'),
    recorded_at   INTEGER NOT NULL,
    PRIMARY KEY (kind, target)
);

INSERT INTO model_legacy_scopes
    (kind, target, family, body, applicability, reason, recorded_at)
SELECT 'body_params',
       look || '@' || version || '/' || body,
       look,
       body,
       'unassessed',
       'epoch_and_sim_unrecorded',
       CAST(strftime('%s', 'now') AS INTEGER)
FROM release_body_params;

INSERT INTO model_legacy_scopes
    (kind, target, family, body, applicability, reason, recorded_at)
SELECT 'pool_state',
       look || '/' || body,
       look,
       body,
       'unassessed',
       'epoch_and_sim_unrecorded',
       CAST(strftime('%s', 'now') AS INTEGER)
FROM look_pool_state;

INSERT INTO model_legacy_scopes
    (kind, target, family, body, applicability, reason, recorded_at)
SELECT 'look_version',
       look || '@' || version,
       look,
       NULL,
       'unassessed',
       'epoch_and_sim_unrecorded',
       CAST(strftime('%s', 'now') AS INTEGER)
FROM look_versions;

CREATE TABLE model_body_states (
    body                   TEXT NOT NULL,
    version                INTEGER NOT NULL CHECK (version > 0),
    stage                  TEXT NOT NULL CHECK (stage IN ('native', 'published')),
    profile_sha256         TEXT,
    input_transform_sha256 TEXT NOT NULL,
    body_params_sha256     TEXT NOT NULL,
    gauge_sha256           TEXT NOT NULL,
    created_at             INTEGER NOT NULL,
    PRIMARY KEY (body, version)
);

CREATE TABLE model_body_state_reservations (
    body        TEXT NOT NULL PRIMARY KEY,
    version     INTEGER NOT NULL CHECK (version > 0),
    stage       TEXT NOT NULL CHECK (stage IN ('native', 'published')),
    reserved_at INTEGER NOT NULL
);

CREATE TABLE model_realization_revisions (
    id                         TEXT NOT NULL PRIMARY KEY,
    subject_key                TEXT NOT NULL REFERENCES model_subjects (key),
    body                       TEXT NOT NULL,
    version                    INTEGER NOT NULL CHECK (version > 0),
    family                     TEXT NOT NULL,
    target_version             INTEGER NOT NULL CHECK (target_version > 0),
    body_state_version         INTEGER NOT NULL CHECK (body_state_version > 0),
    input_transform_version    INTEGER NOT NULL,
    realization_params_version INTEGER NOT NULL,
    control_response_version   INTEGER NOT NULL,
    clip_k                     REAL NOT NULL,
    status                     TEXT NOT NULL CHECK (status IN ('candidate', 'diagnostic')),
    status_reason              TEXT,
    numerical                  TEXT NOT NULL CHECK (numerical IN ('passed', 'failed')),
    release_gates              TEXT NOT NULL CHECK (release_gates IN ('passed', 'failed')),
    created_at                 INTEGER NOT NULL,
    UNIQUE (subject_key, version),
    FOREIGN KEY (family, target_version) REFERENCES look_versions (look, version),
    FOREIGN KEY (body, body_state_version) REFERENCES model_body_states (body, version)
);

CREATE INDEX model_realization_revisions_by_target
    ON model_realization_revisions (family, target_version);

CREATE TABLE model_artifact_bindings (
    owner_kind      TEXT NOT NULL
                    CHECK (owner_kind IN ('realization_revision', 'report', 'family_target',
                                          'target_eligibility', 'bundle_revision')),
    owner_id        TEXT NOT NULL,
    artifact_sha256 TEXT NOT NULL,
    role            TEXT NOT NULL
                    CHECK (role IN ('look', 'control_response', 'bundle', 'manifest', 'card',
                                    'covariance', 'preview', 'report', 'input_transform',
                                    'calibration_profile', 'body_realization_params', 'gauge',
                                    'native_warm_state', 'recipe_output', 'realization_params',
                                    'release', 'fit_resume',
                                    'image_nuisance', 'evidence_membership')),
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (owner_kind, owner_id, artifact_sha256, role)
);

CREATE TABLE model_dependencies (
    owner_kind TEXT NOT NULL
               CHECK (owner_kind IN ('report', 'realization_revision', 'family_head',
                                     'target_eligibility', 'bundle_revision', 'body_state')),
    owner_id   TEXT NOT NULL,
    kind       TEXT NOT NULL
               CHECK (kind IN ('contribution', 'image', 'root_admission', 'transform',
                               'profile', 'schema', 'family_version', 'realization_revision',
                               'rule_version', 'identity_resolution', 'standing', 'cohort',
                               'gauge', 'model_source', 'effective_options', 'probe_set',
                               'partition_policy', 'engine_qualification', 'published_target',
                               'report', 'bundle_revision', 'body_state', 'root_weighting')),
    target     TEXT NOT NULL,
    required   INTEGER NOT NULL CHECK (required IN (0, 1)),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    PRIMARY KEY (owner_kind, owner_id, kind, target)
);

CREATE INDEX model_dependencies_by_target
    ON model_dependencies (kind, target, required);

CREATE TABLE model_dependency_generations (
    kind       TEXT NOT NULL,
    target     TEXT NOT NULL,
    generation INTEGER NOT NULL CHECK (generation >= 0),
    reason     TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (kind, target)
);

CREATE TABLE model_revoked_dependencies (
    kind       TEXT NOT NULL,
    target     TEXT NOT NULL,
    reason     TEXT NOT NULL,
    revoked_at INTEGER NOT NULL,
    PRIMARY KEY (kind, target)
);

CREATE TABLE model_active_numerical_sources (
    id                   INTEGER NOT NULL PRIMARY KEY CHECK (id = 1),
    fitter               TEXT NOT NULL,
    evaluator            TEXT NOT NULL,
    model                TEXT NOT NULL,
    activated_generation INTEGER NOT NULL CHECK (activated_generation >= 1),
    activated_at         INTEGER NOT NULL
);

CREATE TABLE model_invalidation_events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    kind        TEXT NOT NULL,
    target      TEXT NOT NULL,
    reason      TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
);

CREATE TABLE model_subject_revisions (
    subject_key             TEXT NOT NULL REFERENCES model_subjects (key),
    revision                INTEGER NOT NULL CHECK (revision > 0),
    role                    TEXT NOT NULL
                            CHECK (role IN ('target', 'native', 'recipe', 'diagnostic')),
    artifact_sha256         TEXT,
    native_warm_state_sha256 TEXT,
    support                 TEXT NOT NULL DEFAULT '{"probe_set":null,"supported":[],"source_artifact":null}',
    gauge                   REAL,
    gauge_convention        TEXT CHECK (gauge_convention IN ('relative', 'legacy_absolute_header')),
    recorded_at             INTEGER NOT NULL,
    PRIMARY KEY (subject_key, revision)
);

CREATE TABLE model_subject_state (
    subject_key      TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    anchor_revision  INTEGER,
    gauge            REAL,
    gauge_convention TEXT CHECK (gauge_convention IN ('relative', 'legacy_absolute_header')),
    support          TEXT NOT NULL DEFAULT '{"probe_set":null,"supported":[],"source_artifact":null}',
    updated_at       INTEGER NOT NULL
);

CREATE TABLE model_reserved_versions (
    subject_key TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    kind        TEXT NOT NULL CHECK (kind IN ('family', 'realization')),
    version     INTEGER NOT NULL CHECK (version > 0),
    target      TEXT,
    reserved_at INTEGER NOT NULL
);

CREATE TABLE model_fit_attempts (
    subject_key TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    states      TEXT NOT NULL,
    consecutive INTEGER NOT NULL,
    started_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE model_fit_holds (
    subject_key   TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    consecutive   INTEGER NOT NULL,
    first_attempt INTEGER NOT NULL,
    last_attempt  INTEGER NOT NULL,
    diagnostics   TEXT NOT NULL,
    status        TEXT NOT NULL CHECK (status IN ('pending', 'resolved'))
);

CREATE TABLE model_pool_state (
    subject_key TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    fingerprint TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);

CREATE TABLE model_reports (
    id                  TEXT NOT NULL PRIMARY KEY,
    subject_key         TEXT NOT NULL REFERENCES model_subjects (key),
    candidate_kind      TEXT NOT NULL CHECK (candidate_kind IN ('shared_family', 'body_realization')),
    candidate_id        TEXT NOT NULL,
    candidate_target    TEXT NOT NULL,
    candidate_version   INTEGER NOT NULL CHECK (candidate_version > 0),
    capability          TEXT NOT NULL
                        CHECK (capability IN ('native_prediction', 'controls_matching',
                                              'cross_body_admission')),
    scope_key           TEXT NOT NULL,
    scope               TEXT NOT NULL,
    rule_revision       TEXT NOT NULL,
    fitter              TEXT NOT NULL,
    evaluator           TEXT NOT NULL,
    model               TEXT NOT NULL,
    probe_set           TEXT NOT NULL,
    effective_options   TEXT NOT NULL,
    partition_policy    TEXT NOT NULL,
    assessment_seeds    TEXT NOT NULL,
    evidence_membership TEXT NOT NULL,
    outcome             TEXT NOT NULL
                        CHECK (outcome IN ('pass', 'insufficient_evidence', 'fail',
                                           'execution_failure')),
    metrics             TEXT NOT NULL,
    supported_mask      TEXT NOT NULL,
    limitations         TEXT NOT NULL,
    evidence_requests   TEXT NOT NULL,
    created_at          INTEGER NOT NULL
);

CREATE INDEX model_reports_by_subject
    ON model_reports (subject_key, capability, outcome);

CREATE TABLE model_engine_prerequisites (
    engine         TEXT NOT NULL,
    rule_revision  TEXT NOT NULL,
    outcome        TEXT NOT NULL
                   CHECK (outcome IN ('pass', 'insufficient_evidence', 'fail',
                                      'execution_failure')),
    metrics        TEXT NOT NULL,
    supported_mask TEXT NOT NULL,
    limitations    TEXT NOT NULL,
    generation     INTEGER NOT NULL CHECK (generation >= 0),
    recorded_at    INTEGER NOT NULL,
    PRIMARY KEY (engine, rule_revision)
);

CREATE TABLE model_pending_work (
    subject_key TEXT NOT NULL PRIMARY KEY REFERENCES model_subjects (key),
    generation  INTEGER NOT NULL CHECK (generation >= 0),
    updated_at  INTEGER NOT NULL
);

CREATE TABLE model_current_projections (
    subject_key               TEXT NOT NULL REFERENCES model_subjects (key),
    "use"                     TEXT NOT NULL
                              CHECK ("use" IN ('qualified_match', 'experimental_match', 'preview')),
    capability                TEXT NOT NULL
                              CHECK (capability IN ('native_prediction', 'controls_matching',
                                                    'cross_body_admission')),
    scope_key                 TEXT NOT NULL CHECK (scope_key = 'all' OR length(scope_key) = 64),
    selected_report_id        TEXT REFERENCES model_reports (id),
    selected_candidate_kind   TEXT CHECK (selected_candidate_kind IN ('shared_family',
                                                                      'body_realization')),
    selected_candidate_id     TEXT,
    selected_candidate_target TEXT,
    selected_candidate_version INTEGER,
    pending_generation        INTEGER NOT NULL DEFAULT 0 CHECK (pending_generation >= 0),
    applicability             TEXT NOT NULL CHECK (applicability IN ('current', 'invalidated')),
    invalidated_reason        TEXT,
    updated_at                INTEGER NOT NULL,
    PRIMARY KEY (subject_key, "use", capability, scope_key),
    CHECK ((applicability = 'current') = (invalidated_reason IS NULL)),
    CHECK ((selected_candidate_kind IS NULL) = (selected_candidate_id IS NULL)),
    CHECK ((selected_candidate_id IS NULL) = (selected_candidate_target IS NULL)),
    CHECK ((selected_candidate_id IS NULL) = (selected_candidate_version IS NULL))
);

CREATE INDEX model_current_projections_by_report
    ON model_current_projections (selected_report_id);

CREATE TABLE model_family_heads (
    family              TEXT NOT NULL PRIMARY KEY,
    head_version        INTEGER NOT NULL CHECK (head_version > 0),
    source_limited      INTEGER NOT NULL CHECK (source_limited IN (0, 1)),
    numerical           TEXT NOT NULL CHECK (numerical IN ('passed', 'failed')),
    release_gates       TEXT NOT NULL CHECK (release_gates IN ('passed', 'failed')),
    report_id           TEXT REFERENCES model_reports (id),
    assessed_generation INTEGER NOT NULL CHECK (assessed_generation >= 0),
    applicability       TEXT NOT NULL CHECK (applicability IN ('current', 'invalidated')),
    invalidated_reason  TEXT,
    updated_at          INTEGER NOT NULL,
    FOREIGN KEY (family, head_version) REFERENCES look_versions (look, version),
    CHECK ((applicability = 'current') = (invalidated_reason IS NULL))
);

CREATE TABLE model_cohort_revisions (
    family      TEXT NOT NULL,
    revision    INTEGER NOT NULL CHECK (revision > 0),
    role        TEXT NOT NULL CHECK (role IN ('seed', 'admitted')),
    digest      TEXT NOT NULL,
    basis       TEXT NOT NULL CHECK (basis IN ('seed_evidence', 'cross_body_admission')),
    recorded_at INTEGER NOT NULL,
    PRIMARY KEY (family, revision)
);

CREATE TABLE model_family_cohort (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    family           TEXT NOT NULL,
    cohort_revision  INTEGER NOT NULL,
    member_key       TEXT NOT NULL,
    body             TEXT NOT NULL,
    realization_epoch INTEGER,
    realization_sim  TEXT,
    realization_version INTEGER,
    target           TEXT,
    scope_key        TEXT,
    scope            TEXT,
    supported_mask   TEXT,
    report_id        TEXT REFERENCES model_reports (id),
    FOREIGN KEY (family, cohort_revision) REFERENCES model_cohort_revisions (family, revision),
    UNIQUE (family, cohort_revision, member_key)
);

CREATE TABLE model_cohort_admissions (
    family          TEXT NOT NULL,
    target_version  INTEGER NOT NULL CHECK (target_version > 0),
    report_id       TEXT NOT NULL REFERENCES model_reports (id),
    cohort_revision INTEGER NOT NULL CHECK (cohort_revision > 0),
    assessed_domain TEXT NOT NULL,
    recorded_at     INTEGER NOT NULL,
    PRIMARY KEY (family, target_version, report_id),
    FOREIGN KEY (family, cohort_revision) REFERENCES model_cohort_revisions (family, revision)
);

CREATE TABLE model_target_eligibility_snapshots (
    id                         TEXT NOT NULL PRIMARY KEY,
    target                     TEXT NOT NULL,
    basis                      TEXT NOT NULL CHECK (basis IN ('native_family', 'lut_composition')),
    artifacts                  TEXT NOT NULL,
    base_pin                   TEXT,
    support_uncertainty_source TEXT,
    status                     TEXT NOT NULL CHECK (status IN ('eligible', 'ineligible', 'invalidated')),
    reasons                    TEXT NOT NULL,
    recorded_at                INTEGER NOT NULL
);

CREATE TABLE model_target_eligibility_current (
    target      TEXT NOT NULL PRIMARY KEY,
    snapshot_id TEXT NOT NULL REFERENCES model_target_eligibility_snapshots (id),
    updated_at  INTEGER NOT NULL
);

CREATE TABLE model_bundle_revisions (
    id         TEXT NOT NULL PRIMARY KEY,
    body       TEXT NOT NULL,
    epoch      INTEGER NOT NULL,
    manifest   TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE model_bundle_revision_realizations (
    bundle_id      TEXT NOT NULL REFERENCES model_bundle_revisions (id),
    realization_id TEXT NOT NULL,
    PRIMARY KEY (bundle_id, realization_id)
);

CREATE TABLE model_bundle_current (
    body               TEXT NOT NULL,
    epoch              INTEGER NOT NULL,
    "use"              TEXT NOT NULL CHECK ("use" IN ('qualified_match', 'experimental_match')),
    bundle_id          TEXT NOT NULL REFERENCES model_bundle_revisions (id),
    applicability      TEXT NOT NULL CHECK (applicability IN ('current', 'invalidated')),
    invalidated_reason TEXT,
    updated_at         INTEGER NOT NULL,
    PRIMARY KEY (body, epoch, "use"),
    CHECK ((applicability = 'current') = (invalidated_reason IS NULL))
);

CREATE TABLE model_evidence_bounties (
    report_id    TEXT NOT NULL REFERENCES model_reports (id),
    subject_key  TEXT NOT NULL REFERENCES model_subjects (key),
    capability   TEXT NOT NULL
                 CHECK (capability IN ('native_prediction', 'controls_matching',
                                       'cross_body_admission')),
    request_kind TEXT NOT NULL
                 CHECK (request_kind IN ('missing_settings', 'missing_control_values',
                                         'missing_independent_captures',
                                         'missing_calibration_source',
                                         'missing_cross_body_evidence',
                                         'missing_engine_qualification')),
    detail       TEXT NOT NULL,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (report_id, request_kind, detail)
);

CREATE TABLE model_qualified_image_stats (
    subject_key       TEXT NOT NULL REFERENCES model_subjects (key),
    candidate_kind    TEXT NOT NULL CHECK (candidate_kind IN ('shared_family', 'body_realization')),
    candidate_id      TEXT NOT NULL,
    candidate_target  TEXT NOT NULL,
    scope_key         TEXT NOT NULL CHECK (scope_key = 'all' OR length(scope_key) = 64),
    image_id          TEXT NOT NULL,
    rms_de            REAL NOT NULL,
    z                 REAL NOT NULL,
    verdict           TEXT NOT NULL CHECK (verdict IN ('accepted', 'down_weighted', 'quarantined')),
    reason            TEXT,
    weight_components TEXT NOT NULL,
    report_id         TEXT NOT NULL REFERENCES model_reports (id),
    computed_at       INTEGER NOT NULL,
    PRIMARY KEY (subject_key, candidate_kind, candidate_id, candidate_target, scope_key, image_id)
);

CREATE TABLE model_active_evidence_requests (
    subject_key    TEXT NOT NULL REFERENCES model_subjects (key),
    capability     TEXT NOT NULL
                   CHECK (capability IN ('native_prediction', 'controls_matching',
                                         'cross_body_admission')),
    request_kind   TEXT NOT NULL
                   CHECK (request_kind IN ('missing_settings', 'missing_control_values',
                                           'missing_independent_captures',
                                           'missing_calibration_source',
                                           'missing_cross_body_evidence',
                                           'missing_engine_qualification')),
    detail         TEXT NOT NULL,
    observed_groups INTEGER,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (subject_key, capability, request_kind)
);

CREATE TABLE model_diagnostic_image_stats (
    subject_key       TEXT NOT NULL REFERENCES model_subjects (key),
    image_id          TEXT NOT NULL,
    rms_de            REAL NOT NULL,
    z                 REAL NOT NULL,
    verdict           TEXT NOT NULL CHECK (verdict IN ('accepted', 'down_weighted', 'quarantined')),
    reason            TEXT,
    weight_components TEXT NOT NULL,
    computed_at       INTEGER NOT NULL,
    PRIMARY KEY (subject_key, image_id)
);

CREATE TRIGGER model_subjects_no_update BEFORE UPDATE ON model_subjects
BEGIN
    SELECT RAISE(ABORT, 'model_subjects is append-only');
END;
CREATE TRIGGER model_subjects_no_delete BEFORE DELETE ON model_subjects
BEGIN
    SELECT RAISE(ABORT, 'model_subjects is append-only');
END;

CREATE TRIGGER model_legacy_scopes_no_update BEFORE UPDATE ON model_legacy_scopes
BEGIN
    SELECT RAISE(ABORT, 'model_legacy_scopes is append-only');
END;
CREATE TRIGGER model_legacy_scopes_no_delete BEFORE DELETE ON model_legacy_scopes
BEGIN
    SELECT RAISE(ABORT, 'model_legacy_scopes is append-only');
END;

CREATE TRIGGER model_realization_revisions_no_update BEFORE UPDATE ON model_realization_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_realization_revisions is append-only');
END;
CREATE TRIGGER model_realization_revisions_no_delete BEFORE DELETE ON model_realization_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_realization_revisions is append-only');
END;

CREATE TRIGGER model_artifact_bindings_no_update BEFORE UPDATE ON model_artifact_bindings
BEGIN
    SELECT RAISE(ABORT, 'model_artifact_bindings is append-only');
END;
CREATE TRIGGER model_artifact_bindings_no_delete BEFORE DELETE ON model_artifact_bindings
BEGIN
    SELECT RAISE(ABORT, 'model_artifact_bindings is append-only');
END;

CREATE TRIGGER model_dependencies_no_update BEFORE UPDATE ON model_dependencies
BEGIN
    SELECT RAISE(ABORT, 'model_dependencies is append-only');
END;
CREATE TRIGGER model_dependencies_no_delete BEFORE DELETE ON model_dependencies
BEGIN
    SELECT RAISE(ABORT, 'model_dependencies is append-only');
END;

CREATE TRIGGER model_subject_revisions_no_update BEFORE UPDATE ON model_subject_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_subject_revisions is append-only');
END;
CREATE TRIGGER model_subject_revisions_no_delete BEFORE DELETE ON model_subject_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_subject_revisions is append-only');
END;

CREATE TRIGGER model_reports_no_update BEFORE UPDATE ON model_reports
BEGIN
    SELECT RAISE(ABORT, 'model_reports is append-only');
END;
CREATE TRIGGER model_reports_no_delete BEFORE DELETE ON model_reports
BEGIN
    SELECT RAISE(ABORT, 'model_reports is append-only');
END;

CREATE TRIGGER model_engine_prerequisites_no_update BEFORE UPDATE ON model_engine_prerequisites
BEGIN
    SELECT RAISE(ABORT, 'model_engine_prerequisites is append-only');
END;
CREATE TRIGGER model_engine_prerequisites_no_delete BEFORE DELETE ON model_engine_prerequisites
BEGIN
    SELECT RAISE(ABORT, 'model_engine_prerequisites is append-only');
END;

CREATE TRIGGER model_cohort_revisions_no_update BEFORE UPDATE ON model_cohort_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_cohort_revisions is append-only');
END;
CREATE TRIGGER model_cohort_revisions_no_delete BEFORE DELETE ON model_cohort_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_cohort_revisions is append-only');
END;

CREATE TRIGGER model_family_cohort_no_update BEFORE UPDATE ON model_family_cohort
BEGIN
    SELECT RAISE(ABORT, 'model_family_cohort is append-only');
END;
CREATE TRIGGER model_family_cohort_no_delete BEFORE DELETE ON model_family_cohort
BEGIN
    SELECT RAISE(ABORT, 'model_family_cohort is append-only');
END;

CREATE TRIGGER model_cohort_admissions_no_update BEFORE UPDATE ON model_cohort_admissions
BEGIN
    SELECT RAISE(ABORT, 'model_cohort_admissions is append-only');
END;
CREATE TRIGGER model_cohort_admissions_no_delete BEFORE DELETE ON model_cohort_admissions
BEGIN
    SELECT RAISE(ABORT, 'model_cohort_admissions is append-only');
END;

CREATE TRIGGER model_target_eligibility_snapshots_no_update
BEFORE UPDATE ON model_target_eligibility_snapshots
BEGIN
    SELECT RAISE(ABORT, 'model_target_eligibility_snapshots is append-only');
END;
CREATE TRIGGER model_target_eligibility_snapshots_no_delete
BEFORE DELETE ON model_target_eligibility_snapshots
BEGIN
    SELECT RAISE(ABORT, 'model_target_eligibility_snapshots is append-only');
END;

CREATE TRIGGER model_body_states_no_update BEFORE UPDATE ON model_body_states
BEGIN
    SELECT RAISE(ABORT, 'model_body_states is append-only');
END;
CREATE TRIGGER model_body_states_no_delete BEFORE DELETE ON model_body_states
BEGIN
    SELECT RAISE(ABORT, 'model_body_states is append-only');
END;

CREATE TRIGGER model_bundle_revisions_no_update BEFORE UPDATE ON model_bundle_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_bundle_revisions is append-only');
END;
CREATE TRIGGER model_bundle_revisions_no_delete BEFORE DELETE ON model_bundle_revisions
BEGIN
    SELECT RAISE(ABORT, 'model_bundle_revisions is append-only');
END;

CREATE TRIGGER model_bundle_revision_realizations_no_update
BEFORE UPDATE ON model_bundle_revision_realizations
BEGIN
    SELECT RAISE(ABORT, 'model_bundle_revision_realizations is append-only');
END;
CREATE TRIGGER model_bundle_revision_realizations_no_delete
BEFORE DELETE ON model_bundle_revision_realizations
BEGIN
    SELECT RAISE(ABORT, 'model_bundle_revision_realizations is append-only');
END;

CREATE TRIGGER model_invalidation_events_no_update BEFORE UPDATE ON model_invalidation_events
BEGIN
    SELECT RAISE(ABORT, 'model_invalidation_events is append-only');
END;
CREATE TRIGGER model_invalidation_events_no_delete BEFORE DELETE ON model_invalidation_events
BEGIN
    SELECT RAISE(ABORT, 'model_invalidation_events is append-only');
END;

DROP VIEW blob_references;
CREATE VIEW blob_references AS
    SELECT lower(hex(hash)) AS hash FROM blobs
    UNION
    SELECT lower(hex(hash)) FROM blob_staging
    UNION
    SELECT lower(hex(sha256)) FROM fixture_uploads
    UNION
    SELECT lower(hex(sha256)) FROM lut_uploads
    UNION
    SELECT lower(json_extract(artifact.value, '$.sha256'))
      FROM version_manifests, json_each(version_manifests.artifacts) AS artifact
    UNION
    SELECT lower(artifact_sha256) FROM model_artifact_bindings
    UNION
    SELECT lower(profile_sha256) FROM model_body_states
      WHERE profile_sha256 IS NOT NULL
    UNION
    SELECT lower(input_transform_sha256) FROM model_body_states
    UNION
    SELECT lower(body_params_sha256) FROM model_body_states
    UNION
    SELECT lower(gauge_sha256) FROM model_body_states
    UNION
    SELECT lower(artifact_sha256) FROM model_subject_revisions
      WHERE artifact_sha256 IS NOT NULL
    UNION
    SELECT lower(native_warm_state_sha256) FROM model_subject_revisions
      WHERE native_warm_state_sha256 IS NOT NULL
    UNION
    SELECT lower(json_extract(artifact.value, '$.sha256'))
      FROM model_target_eligibility_snapshots,
           json_each(model_target_eligibility_snapshots.artifacts) AS artifact;

CREATE TRIGGER model_revoked_dependencies_no_update BEFORE UPDATE ON model_revoked_dependencies
BEGIN
    SELECT RAISE(ABORT, 'model_revoked_dependencies is append-only');
END;
CREATE TRIGGER model_revoked_dependencies_no_delete BEFORE DELETE ON model_revoked_dependencies
BEGIN
    SELECT RAISE(ABORT, 'model_revoked_dependencies is append-only');
END;
