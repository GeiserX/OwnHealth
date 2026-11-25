-- A completed Garmin import retains the inbox hash for deduplication and releases the raw bytes.
alter table source_item add column raw_released_at timestamptz;
