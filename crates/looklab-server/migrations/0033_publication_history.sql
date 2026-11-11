-- Durable publication history for installed input priors and interface profiles.

CREATE TABLE published_input_transforms (
    body       TEXT NOT NULL,
    version    INTEGER NOT NULL CHECK (version > 0),
    sha256     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (body, sha256)
);

CREATE INDEX published_input_transforms_by_body
    ON published_input_transforms (body, version);

CREATE TABLE published_interface_revisions (
    manifest_sha256   TEXT NOT NULL PRIMARY KEY,
    definition_sha256 TEXT NOT NULL,
    report_sha256     TEXT NOT NULL,
    created_at        INTEGER NOT NULL
);

CREATE TRIGGER published_input_transforms_no_update
BEFORE UPDATE ON published_input_transforms
BEGIN
    SELECT RAISE(ABORT, 'published_input_transforms is append-only');
END;
CREATE TRIGGER published_input_transforms_no_delete
BEFORE DELETE ON published_input_transforms
BEGIN
    SELECT RAISE(ABORT, 'published_input_transforms is append-only');
END;
CREATE TRIGGER published_interface_revisions_no_update
BEFORE UPDATE ON published_interface_revisions
BEGIN
    SELECT RAISE(ABORT, 'published_interface_revisions is append-only');
END;
CREATE TRIGGER published_interface_revisions_no_delete
BEFORE DELETE ON published_interface_revisions
BEGIN
    SELECT RAISE(ABORT, 'published_interface_revisions is append-only');
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
           json_each(model_target_eligibility_snapshots.artifacts) AS artifact
    UNION
    SELECT lower(sha256) FROM published_input_transforms
    UNION
    SELECT lower(manifest_sha256) FROM published_interface_revisions
    UNION
    SELECT lower(definition_sha256) FROM published_interface_revisions
    UNION
    SELECT lower(report_sha256) FROM published_interface_revisions;
