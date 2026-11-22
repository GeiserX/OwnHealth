-- Open Wearables' own Postgres role and database, for the `wearables` profile of compose.yaml and
-- deploy/compose.homelab.yaml. The one-shot `ow-db-init` service runs it as the database owner before
-- the sidecar starts, every time. Safe to run any number of times.
--
--   psql variables: ow_role (the role name), ow_db (its database)
--   environment:    OPENWEARABLES_DB_PASSWORD (its password, read with \getenv so it never sits in argv):
--                   from openwearables.env (compose.yaml) or secrets/ow_db_password (homelab file)
--
-- The sidecar is a cache of vendor data, and it holds the vendor tokens. Its role is a plain login: no
-- superuser, no CREATEDB, no CREATEROLE. It owns its own database, so OW's migrations can create their
-- tables there, and it can connect to no other database: this script takes PUBLIC's default CONNECT
-- away from every other database of this server (ownhealth, postgres, template1). So run it only
-- against the Postgres that the compose file bundles, never a shared server.
\getenv ow_password OPENWEARABLES_DB_PASSWORD
\if :{?ow_password}
\else
  do $$ begin raise exception 'OPENWEARABLES_DB_PASSWORD is not set: put it in openwearables.env (compose.yaml) or secrets/ow_db_password (homelab)'; end $$;
\endif
select :'ow_password' = '' as ow_password_empty \gset
\if :ow_password_empty
  do $$ begin raise exception 'OPENWEARABLES_DB_PASSWORD is empty: put it in openwearables.env (compose.yaml) or secrets/ow_db_password (homelab)'; end $$;
\endif

select format('create role %I login', :'ow_role')
 where not exists (select 1 from pg_roles where rolname = :'ow_role') \gexec
select format('alter role %I login nosuperuser nocreatedb nocreaterole noreplication nobypassrls password %L',
              :'ow_role', :'ow_password') \gexec
select format('create database %I owner %I', :'ow_db', :'ow_role')
 where not exists (select 1 from pg_database where datname = :'ow_db') \gexec
select format('alter database %I owner to %I', :'ow_db', :'ow_role') \gexec
select format('revoke all on database %I from public', :'ow_db') \gexec
select format('revoke connect on database %I from public', datname)
  from pg_database where datallowconn and datname <> :'ow_db' \gexec

-- Check the fence: the role may connect to its own database and to nothing else.
select coalesce(string_agg(datname, ', ' order by datname), '') as ow_other_databases,
       count(*) > 0 as ow_fence_open
  from pg_database
 where datallowconn and datname <> :'ow_db' and has_database_privilege(:'ow_role', oid, 'CONNECT') \gset
\if :ow_fence_open
  \echo 'the Open Wearables role can still connect to:' :ow_other_databases
  do $$ begin raise exception 'the Open Wearables role can connect to another database'; end $$;
\endif
