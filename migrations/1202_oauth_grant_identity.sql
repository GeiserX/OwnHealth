-- A reconnect replaces the grant even when it reuses the same secret row.
-- Null denotes a legacy grant or a source that does not use OAuth.
alter table source add column oauth_grant_id uuid;
alter table source_run add column oauth_grant_id uuid;
