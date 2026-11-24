-- ICE cards use the share listing, access log and expiry sweep. I marks an offline-only card.
-- LU marks a direct, live SMART Health Link. Its file row holds a per-card key sealed under the
-- profile DEK. It has no cached JWE. Revocation and expiry delete the file row and its sealed key.
alter table shl drop constraint shl_flag_check;
alter table shl add constraint shl_flag_check check (flag in ('', 'P', 'I', 'LU'));
-- A new emergency card ends the previous one with reason `replaced`.
alter table shl drop constraint shl_revoked_reason_check;
alter table shl add constraint shl_revoked_reason_check
  check (revoked_reason in ('person', 'locked', 'access_changed', 'replaced'));

alter table shl_file alter column jwe drop not null;
alter table shl_file add column ice_key bytea;
alter table shl_file add constraint shl_file_material_check check ((jwe is null) <> (ice_key is null));

-- The ordinary SHL trigger accepts guardians. An ICE card can only be issued by an owner.
create function ice_end_after_owner_change() returns trigger language plpgsql as $$
begin
  with ended as (
    update shl set revoked_at = now(), revoked_reason = 'access_changed'
    where profile_id = old.profile_id and created_by = old.user_id and flag in ('I', 'LU')
      and revoked_at is null and expires_at > clock_timestamp()
    returning id
  )
  delete from shl_file f using ended e where f.shl_id = e.id;
  return null;
end $$;

create trigger ice_owner_change after update of role on profile_grant
  for each row when (old.role = 'owner' and new.role <> 'owner') execute function ice_end_after_owner_change();

alter table notification_outbox drop constraint notification_outbox_kind_check;
alter table notification_outbox add constraint notification_outbox_kind_check
  check (kind in ('break_glass.request', 'break_glass.cancel', 'break_glass.activate', 'break_glass.use',
                  'break_glass.expire', 'break_glass.revoke', 'ice.access'));
