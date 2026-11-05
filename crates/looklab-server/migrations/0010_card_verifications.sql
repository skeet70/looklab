-- Card-render verification per firmware and Look version ([docs/04 §8.4]).

CREATE TABLE card_verifications (
    firmware_id      TEXT NOT NULL,
    firmware_version TEXT NOT NULL,
    look_slug        TEXT NOT NULL,
    look_version     TEXT NOT NULL,
    images           INTEGER NOT NULL,
    median_de        REAL NOT NULL,
    p95_de           REAL NOT NULL,
    updated_at       INTEGER NOT NULL,
    PRIMARY KEY (firmware_id, firmware_version, look_slug, look_version)
);

CREATE INDEX card_verifications_by_firmware
    ON card_verifications (firmware_id, firmware_version);
