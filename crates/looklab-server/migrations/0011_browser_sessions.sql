-- SPDX-License-Identifier: AGPL-3.0-or-later

DROP INDEX status_tokens_one_active;
CREATE INDEX status_tokens_active_credential
    ON status_tokens (credential_id) WHERE revoked_at IS NULL;
