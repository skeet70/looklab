-- Owner withdrawal fences the camera's confirmations on sends that already exist.

CREATE TABLE manual_confirmation_blocked_sends (
    credential_id TEXT NOT NULL,
    camera_id     TEXT NOT NULL,
    send_nonce    TEXT NOT NULL,
    PRIMARY KEY (credential_id, camera_id, send_nonce)
);
