-- A withdrawn confirmation no longer counts toward a control's agreement or
-- contradiction state. The row stays for audit; the digest is not reusable.

ALTER TABLE manual_confirmations ADD COLUMN withdrawn_at INTEGER;

CREATE INDEX manual_confirmations_live
    ON manual_confirmations (camera_id, withdrawn_at);
