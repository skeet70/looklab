-- The composition base a source-only preview was baked with, so later base
-- publications can skip re-marking a preview that already uses the best base.

ALTER TABLE lut_base_pins ADD COLUMN preview_base TEXT;
