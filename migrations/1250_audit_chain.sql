-- v1.0 audit: the session-report columns and the hash chain (security-privacy.md section 9).

-- The agent session report fills these behind its consent switch: how many not_assessed entries a
-- call answered with, and how wide its arguments were. Counts only, never the arguments.
alter table audit_event add column not_assessed_n int, add column arg_bytes int;

-- One link per audit_event, in commit order: hash = sha256(prev_hash || oh_audit_row_v1(event)),
-- and the first link's prev_hash is 32 zero bytes. Append-only, like audit_event. Only the trigger
-- below writes links, as the owner: deploy/db/app-role.sql leaves the app role SELECT alone here.
-- No foreign key to audit_event: one would answer a TRUNCATE of audit_event before its append-only
-- trigger does, and verify_chain names a link whose event is gone anyway.
create table audit_chain (
  seq bigint primary key,
  event_id bigint not null unique,
  prev_hash bytea not null,
  hash bytea not null);
create trigger audit_chain_no_update before update or delete on audit_chain
  for each row execute function oh_append_only();
create trigger audit_chain_no_truncate before truncate on audit_chain
  for each statement execute function oh_append_only();

-- The bytes a link hashes, version 1: an explicit column list in a fixed order, the time in UTC,
-- never select *. A column added to audit_event later joins the hash only through a v2 of this.
create function oh_audit_row_v1(e audit_event) returns bytea language sql stable as $$
  select convert_to(jsonb_build_array('v1', e.id, e.ts at time zone 'UTC', e.actor_kind, e.actor_id,
    e.token_id, e.client, e.via, e.action, e.tool, e.profile_id, e.resource_type, e.resource_ids,
    e.src_ids, e.args_summary, e.result_kind, e.rows_out, e.bytes_out, e.withheld,
    e.protocol_version, e.ip::text, e.user_agent, e.not_assessed_n, e.arg_bytes)::text, 'UTF8')
$$;

-- Links the event at commit. The transaction-scoped lock makes commits take the head one at a time,
-- and it is taken only at commit, after the transaction's own work, so it waits on nothing else.
-- A repeatable read snapshot cannot see a link committed after it began: it would take a stale head
-- and fail on seq only when another commit raced it, so such a transaction is refused every time.
-- It runs as the owner, so the app role needs no right on audit_chain. No other role may call it or
-- attach it to a table (PUBLIC loses EXECUTE below), it looks names up in pg_catalog only, and it
-- refuses every table but public.audit_event before it reads the row: a row of another table could
-- reach a cast to audit_event that its creator wrote, and that cast would then run as the owner.
create function oh_audit_chain_link() returns trigger language plpgsql
  security definer set search_path = pg_catalog, pg_temp as $$
declare
  head public.audit_chain;
  prev bytea;
begin
  if tg_relid <> 'public.audit_event'::regclass then
    raise exception 'oh_audit_chain_link links public.audit_event only'
      using errcode = 'wrong_object_type';
  end if;
  if current_setting('transaction_isolation') <> 'read committed' then
    raise exception 'audit_event is written only in read committed transactions'
      using errcode = 'feature_not_supported';
  end if;
  perform pg_advisory_xact_lock(hashtext('ownhealth.audit_chain'));
  select * into head from public.audit_chain order by seq desc limit 1;
  prev := coalesce(head.hash, decode(repeat('00', 32), 'hex'));
  insert into public.audit_chain (seq, event_id, prev_hash, hash)
    values (coalesce(head.seq, 0) + 1, new.id, prev, sha256(prev || public.oh_audit_row_v1(new)));
  return null;
end $$;
revoke all on function oh_audit_chain_link() from public;
create constraint trigger audit_event_chain after insert on audit_event
  deferrable initially deferred for each row execute function oh_audit_chain_link();

-- The rows already written, in id order. The ALTER TABLE above holds an ACCESS EXCLUSIVE lock on
-- audit_event until this migration commits, so a running older app cannot add an event the loop misses.
do $$
declare
  e audit_event;
  prev bytea := decode(repeat('00', 32), 'hex');
  n bigint := 0;
  h bytea;
begin
  for e in select * from audit_event order by id loop
    n := n + 1;
    h := sha256(prev || oh_audit_row_v1(e));
    insert into audit_chain (seq, event_id, prev_hash, hash) values (n, e.id, prev, h);
    prev := h;
  end loop;
end $$;
