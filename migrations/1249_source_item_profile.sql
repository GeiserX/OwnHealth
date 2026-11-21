-- The profile an inbox filed each item to, so a profile delete finds the raw bytes of an item that made
-- no document (a mail with only a body keeps its .eml). Null on items filed to nobody (held mail) and on
-- rows older than this file. A profile delete finds such an older row only another way: through its
-- document, a mail released to the profile with a document, the profile's own inbox, or a document of the
-- profile with the same bytes. An older mail with only a body, from a household mailbox, has none of
-- these, so a profile delete leaves its .eml.
alter table source_item add column profile_id uuid references profile on delete set null;
create index source_item_profile_idx on source_item (profile_id) where profile_id is not null;
