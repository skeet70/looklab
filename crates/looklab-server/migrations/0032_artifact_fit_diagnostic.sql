-- Diagnostic fit payloads had no role of their own and were stored as fit resumes.
-- SQLite cannot widen a CHECK constraint in place, so rebuild the binding table.

DROP VIEW blob_references;

CREATE TABLE model_artifact_bindings_new (
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
                                    'release', 'fit_resume', 'fit_diagnostic',
                                    'image_nuisance', 'evidence_membership')),
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (owner_kind, owner_id, artifact_sha256, role)
);

INSERT INTO model_artifact_bindings_new
    (owner_kind, owner_id, artifact_sha256, role, created_at)
SELECT owner_kind, owner_id, artifact_sha256, role, created_at
FROM model_artifact_bindings;

DROP TABLE model_artifact_bindings;
ALTER TABLE model_artifact_bindings_new RENAME TO model_artifact_bindings;

CREATE TRIGGER model_artifact_bindings_no_update BEFORE UPDATE ON model_artifact_bindings
BEGIN
    SELECT RAISE(ABORT, 'model_artifact_bindings is append-only');
END;
CREATE TRIGGER model_artifact_bindings_no_delete BEFORE DELETE ON model_artifact_bindings
BEGIN
    SELECT RAISE(ABORT, 'model_artifact_bindings is append-only');
END;

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
