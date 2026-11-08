-- Lane F: per-token MCP limits (mcp.md 9.4). Counters live in Postgres so they survive restarts and
-- hold across workers. One row per token per day for the daily budgets, one row per token for the
-- per-minute call bucket.

create table mcp_usage (
  token_id uuid not null references api_token on delete cascade,
  day date not null,                 -- in the install's timezone; resets at 00:00 there
  calls int not null default 0,
  facts int not null default 0,      -- rows with a src id returned
  bytes bigint not null default 0,   -- response bytes returned
  lookups int not null default 0,    -- lookup_variant calls
  proposals int not null default 0,  -- propose calls that created a review item
  primary key (token_id, day));

create table mcp_rate (              -- token bucket: refills rate_per_min per minute, holds at most the burst
  token_id uuid primary key references api_token on delete cascade,
  tokens double precision not null,
  updated_at timestamptz not null);
