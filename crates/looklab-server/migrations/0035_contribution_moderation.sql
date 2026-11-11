-- Contribution-scoped moderation and future-intake blocks (docs/05 §8).

CREATE TABLE contribution_rejections (
    contribution_id      TEXT NOT NULL PRIMARY KEY REFERENCES contributions (id),
    moderator_credential TEXT NOT NULL,
    reason               TEXT NOT NULL,
    rejected_at          INTEGER NOT NULL
);

-- Denies new contributions without revoking the credential; withdrawal, status
-- reads and existing data stay available.
CREATE TABLE credential_intake_blocks (
    credential_id        TEXT NOT NULL PRIMARY KEY REFERENCES credentials (id),
    moderator_credential TEXT NOT NULL,
    reason               TEXT NOT NULL,
    blocked_at           INTEGER NOT NULL
);

CREATE TRIGGER contribution_rejections_no_update BEFORE UPDATE ON contribution_rejections
BEGIN
    SELECT RAISE(ABORT, 'contribution_rejections is append-only');
END;

CREATE TRIGGER contribution_rejections_no_delete BEFORE DELETE ON contribution_rejections
BEGIN
    SELECT RAISE(ABORT, 'contribution_rejections is append-only');
END;

CREATE TRIGGER credential_intake_blocks_no_update BEFORE UPDATE ON credential_intake_blocks
BEGIN
    SELECT RAISE(ABORT, 'credential_intake_blocks is append-only');
END;

CREATE TRIGGER credential_intake_blocks_no_delete BEFORE DELETE ON credential_intake_blocks
BEGIN
    SELECT RAISE(ABORT, 'credential_intake_blocks is append-only');
END;

CREATE TRIGGER contribution_rejection_sticky
BEFORE UPDATE OF status ON contributions
WHEN OLD.status = 'quarantined'
 AND NEW.status NOT IN ('quarantined', 'withdrawn')
 AND EXISTS (
        SELECT 1 FROM contribution_rejections
        WHERE contribution_id = OLD.id)
BEGIN
    SELECT RAISE(IGNORE);
END;

CREATE TRIGGER contribution_image_rejection_sticky
BEFORE UPDATE OF status ON contribution_images
WHEN NEW.status != 'quarantined'
 AND EXISTS (
        SELECT 1 FROM contribution_rejections
        WHERE contribution_id = OLD.contribution_id)
BEGIN
    SELECT RAISE(IGNORE);
END;
