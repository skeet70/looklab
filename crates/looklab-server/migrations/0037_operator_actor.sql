-- The offline operator CLI admits a custom look without a credential row.
-- Grants already record that actor as the literal `cli`, so the decision and
-- upload tables stop requiring a credential reference for it.

DROP VIEW blob_references;

CREATE TABLE moderation_decisions_new (
    id                   TEXT NOT NULL PRIMARY KEY,
    proposal_id          TEXT NOT NULL REFERENCES proposals (id),
    moderator_credential TEXT NOT NULL,
    decision             TEXT NOT NULL,
    reason               TEXT NOT NULL,
    decided_at           INTEGER NOT NULL
);

INSERT INTO moderation_decisions_new
    (id, proposal_id, moderator_credential, decision, reason, decided_at)
SELECT id, proposal_id, moderator_credential, decision, reason, decided_at
FROM moderation_decisions;

DROP TABLE moderation_decisions;
ALTER TABLE moderation_decisions_new RENAME TO moderation_decisions;

CREATE INDEX moderation_decisions_by_proposal ON moderation_decisions (proposal_id, decided_at);

CREATE TABLE lut_uploads_new (
    proposal_id   TEXT NOT NULL PRIMARY KEY REFERENCES proposals (id),
    sha256        BLOB NOT NULL,
    credential_id TEXT REFERENCES credentials (id),
    file          TEXT NOT NULL,
    size          INTEGER NOT NULL,
    created_at    INTEGER NOT NULL
);

INSERT INTO lut_uploads_new
    (proposal_id, sha256, credential_id, file, size, created_at)
SELECT proposal_id, sha256, credential_id, file, size, created_at
FROM lut_uploads;

DROP TABLE lut_uploads;
ALTER TABLE lut_uploads_new RENAME TO lut_uploads;

CREATE INDEX lut_uploads_by_credential ON lut_uploads (credential_id, created_at);
CREATE INDEX lut_uploads_by_hash ON lut_uploads (sha256);

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
