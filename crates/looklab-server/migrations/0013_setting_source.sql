-- Per-control claim provenance of each image's settings: a JSON array of
-- `[control key, "reproduced" | "declared"]`, sorted by key.

ALTER TABLE contribution_images ADD COLUMN setting_source TEXT NOT NULL DEFAULT '[]';
