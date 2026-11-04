-- Scope columns hold `{rules: [{body, look}]}`, validated by `looklab_trust::Scope`.
-- The old OR form `{bodies, looks}` migrates to `(body, *)` and `(*, look)` rules.

CREATE TABLE grants_new (
    id                 TEXT NOT NULL PRIMARY KEY,
    subject_credential TEXT NOT NULL REFERENCES credentials (id),
    role_or_tier       TEXT NOT NULL CHECK (role_or_tier IN ('seeder', 'moderator', 'admin')),
    scope              TEXT,
    granted_by         TEXT NOT NULL,
    reason             TEXT NOT NULL,
    granted_at         INTEGER NOT NULL,
    revoked_at         INTEGER
);

INSERT INTO grants_new
    (id, subject_credential, role_or_tier, scope, granted_by, reason, granted_at, revoked_at)
SELECT
    id,
    subject_credential,
    role_or_tier,
    CASE
        WHEN scope IS NULL THEN NULL
        WHEN json_valid(scope) = 0 THEN NULL
        WHEN json_extract(scope, '$.rules') IS NOT NULL THEN scope
        ELSE json_object(
            'rules',
            COALESCE(
                (
                    SELECT json_group_array(json(rule))
                    FROM (
                        SELECT json_object('body', value, 'look', NULL) AS rule
                        FROM json_each(
                            COALESCE(json_extract(grants.scope, '$.bodies'), json('[]'))
                        )
                        UNION ALL
                        SELECT json_object('body', NULL, 'look', value) AS rule
                        FROM json_each(
                            COALESCE(json_extract(grants.scope, '$.looks'), json('[]'))
                        )
                    )
                ),
                json('[]')
            )
        )
    END,
    granted_by,
    reason,
    granted_at,
    revoked_at
FROM grants;

DROP TABLE grants;
ALTER TABLE grants_new RENAME TO grants;

CREATE TABLE endorsements_new (
    id                 TEXT NOT NULL PRIMARY KEY,
    issuer_credential  TEXT NOT NULL REFERENCES credentials (id),
    subject_credential TEXT NOT NULL REFERENCES credentials (id),
    scope              TEXT NOT NULL,
    reason             TEXT NOT NULL,
    evidence_refs      TEXT NOT NULL DEFAULT '[]',
    issued_at          INTEGER NOT NULL,
    revoked_at         INTEGER
);

INSERT INTO endorsements_new
    (id, issuer_credential, subject_credential, scope, reason, evidence_refs, issued_at, revoked_at)
SELECT
    id,
    issuer_credential,
    subject_credential,
    CASE
        WHEN json_valid(scope) = 0 THEN json('{"rules":[]}')
        WHEN json_extract(scope, '$.rules') IS NOT NULL THEN scope
        ELSE json_object(
            'rules',
            COALESCE(
                (
                    SELECT json_group_array(json(rule))
                    FROM (
                        SELECT json_object('body', value, 'look', NULL) AS rule
                        FROM json_each(
                            COALESCE(json_extract(endorsements.scope, '$.bodies'), json('[]'))
                        )
                        UNION ALL
                        SELECT json_object('body', NULL, 'look', value) AS rule
                        FROM json_each(
                            COALESCE(json_extract(endorsements.scope, '$.looks'), json('[]'))
                        )
                    )
                ),
                json('[]')
            )
        )
    END,
    reason,
    evidence_refs,
    issued_at,
    revoked_at
FROM endorsements;

DROP TABLE endorsements;
ALTER TABLE endorsements_new RENAME TO endorsements;

CREATE TABLE root_admissions_new (
    id              TEXT NOT NULL PRIMARY KEY,
    admitted_by     TEXT NOT NULL REFERENCES credentials (id),
    contribution_id TEXT NOT NULL,
    scope           TEXT NOT NULL,
    reason          TEXT NOT NULL,
    admitted_at     INTEGER NOT NULL,
    revoked_at      INTEGER
);

INSERT INTO root_admissions_new
    (id, admitted_by, contribution_id, scope, reason, admitted_at, revoked_at)
SELECT
    id,
    admitted_by,
    contribution_id,
    CASE
        WHEN json_valid(scope) = 0 THEN json('{"rules":[]}')
        WHEN json_extract(scope, '$.rules') IS NOT NULL THEN scope
        ELSE json_object(
            'rules',
            COALESCE(
                (
                    SELECT json_group_array(json(rule))
                    FROM (
                        SELECT json_object('body', value, 'look', NULL) AS rule
                        FROM json_each(
                            COALESCE(json_extract(root_admissions.scope, '$.bodies'), json('[]'))
                        )
                        UNION ALL
                        SELECT json_object('body', NULL, 'look', value) AS rule
                        FROM json_each(
                            COALESCE(json_extract(root_admissions.scope, '$.looks'), json('[]'))
                        )
                    )
                ),
                json('[]')
            )
        )
    END,
    reason,
    admitted_at,
    revoked_at
FROM root_admissions;

DROP TABLE root_admissions;
ALTER TABLE root_admissions_new RENAME TO root_admissions;

CREATE INDEX grants_by_subject ON grants (subject_credential, revoked_at);
CREATE INDEX endorsements_by_subject ON endorsements (subject_credential, revoked_at);
CREATE INDEX root_admissions_by_scope ON root_admissions (revoked_at);

CREATE TABLE look_versions (
    id         TEXT NOT NULL PRIMARY KEY,
    look       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    status     TEXT NOT NULL DEFAULT 'provisional'
                       CHECK (status IN ('provisional', 'rooted', 'published', 'deprecated')),
    changelog  TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (look, version)
);

CREATE INDEX look_versions_by_look ON look_versions (look, version);

CREATE TABLE version_manifests (
    look_version_id TEXT NOT NULL PRIMARY KEY REFERENCES look_versions (id),
    artifacts       TEXT NOT NULL,
    created_at      INTEGER NOT NULL
);

CREATE TABLE version_depends_on_root (
    look_version_id    TEXT NOT NULL REFERENCES look_versions (id),
    root_admission_id  TEXT NOT NULL REFERENCES root_admissions (id),
    PRIMARY KEY (look_version_id, root_admission_id)
);

CREATE INDEX version_depends_by_root ON version_depends_on_root (root_admission_id);

CREATE TABLE version_regating (
    look_version_id TEXT NOT NULL PRIMARY KEY REFERENCES look_versions (id),
    reason          TEXT NOT NULL,
    marked_at       INTEGER NOT NULL
);

CREATE TABLE gate_failures (
    id         TEXT NOT NULL PRIMARY KEY,
    look       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    report     TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'resolved')),
    created_at INTEGER NOT NULL
);

CREATE INDEX gate_failures_pending ON gate_failures (status, created_at);
