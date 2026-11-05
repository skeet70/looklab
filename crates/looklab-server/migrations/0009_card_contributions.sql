-- Card intent fields: firmware id and version, look version, content-id prefix and
-- whether the tag was present ([docs/03 §1], [docs/04 §8.4]).

ALTER TABLE contributions ADD COLUMN card TEXT;
