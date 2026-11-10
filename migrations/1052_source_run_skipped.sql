-- What a push held that OwnHealth does not chart yet, counted per metric ({"blood_glucose": 3}), so a
-- run can say "4 of 6 metrics" instead of dropping the rest without a word. Counts only, never values.
alter table source_run add column skipped jsonb not null default '{}';
