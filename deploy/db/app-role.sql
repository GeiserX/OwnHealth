-- The app's own Postgres role: it reads and writes rows, owns nothing, and cannot change history.
-- Run as the database owner after every migration (app-role.sh does). Safe to run any number of times.
--
--   psql variables: app_role (the role name)
--   environment:    OWNHEALTH_APP_ROLE_PASSWORD (its password, read with \getenv so it never sits in argv)
--
-- The owner keeps the tables, so only the owner can ALTER, DISABLE TRIGGER or DROP them. The app role
-- gets SELECT, INSERT, UPDATE, DELETE on every table, and loses UPDATE, DELETE and TRUNCATE on every
-- table guarded by oh_append_only() (audit_event): security-privacy.md section 9.
\getenv app_password OWNHEALTH_APP_ROLE_PASSWORD
\if :{?app_password}
\else
  do $$ begin raise exception 'OWNHEALTH_APP_ROLE_PASSWORD is not set'; end $$;
\endif

select format('create role %I login', :'app_role')
 where not exists (select 1 from pg_roles where rolname = :'app_role') \gexec
select format('alter role %I login nosuperuser nocreatedb nocreaterole noreplication nobypassrls password %L',
              :'app_role', :'app_password') \gexec
select format('grant connect, temporary on database %I to %I', current_database(), :'app_role') \gexec
select format('grant usage on schema public to %I', :'app_role') \gexec
select format('grant select, insert, update, delete on all tables in schema public to %I', :'app_role') \gexec
select format('grant usage, select on all sequences in schema public to %I', :'app_role') \gexec
select distinct format('revoke update, delete, truncate on %s from %I', t.tgrelid::regclass, :'app_role')
  from pg_trigger t join pg_proc p on p.oid = t.tgfoid
 where p.proname = 'oh_append_only' \gexec
