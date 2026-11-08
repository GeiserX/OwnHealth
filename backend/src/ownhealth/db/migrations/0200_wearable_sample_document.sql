-- Lane C: the export a wearable sample came from, so retracting or moving that export removes its
-- samples. Nullable (manual and demo samples have no document) and without a foreign key, so the
-- millions of rows a large export inserts pay no per-row lookup. The first export to bring a sample
-- owns it: a later export holding the same sample inserts nothing (the unique key).
alter table wearable_sample add column document_id uuid;
