-- The binding becomes a tombstone when the source is removed, including on profile deletion.
-- Its sealed credential stays available until the sidecar confirms deletion or returns 404.
create table openwearables_user (
  user_id uuid primary key,
  source_id uuid unique references source on delete set null,
  secret_id uuid not null unique references secret on delete cascade
);
create index openwearables_user_pending_delete on openwearables_user (user_id) where source_id is null;
