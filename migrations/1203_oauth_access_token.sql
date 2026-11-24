-- A worker can use the grant only after its encrypted access token and expiry commit.
alter table source add column oauth_access_token bytea
    check (oauth_access_token is null or access_expires_at is not null);
