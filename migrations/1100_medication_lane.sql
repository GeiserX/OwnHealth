-- 1100 medication lane (v0.3 records block 1100-1109): what a doctor asks about a medicine beyond the
-- dose. All nullable, no new foreign key; the lifecycle trigger keeps them immutable like every other
-- column, so a new schedule or prescriber is a new version.
--   schedule_text: when it is taken, as written ("twice daily", "before breakfast").
--   prescriber:    who prescribed it, as written.
--   codes:         every coding the source sent, as received: [{"system", "code", "display"}]. The
--                  primary code stays in code_system/code.
alter table medication_statement
  add column schedule_text text,
  add column prescriber text,
  add column codes jsonb check (codes is null or jsonb_typeof(codes) = 'array');
