//! Nightscout pull (lane D): the person's own site, a read token (secret purpose `nightscout_token`),
//! `GET /api/v1/entries/sgv.json?find[date][$gt]=<cursor>&count=...` (and the v3 API where present)
//! through `core::net::get` with `Egress::home_service(allow_private)`. Readings become CGM samples
//! (`cgm::METRIC`); the cursor is the last reading's epoch ms. Source `config` (kind `nightscout`):
//! `{base_url, allow_private?: bool, api?: "v1"|"v3"}`.

use ownhealth_core::error::Error;
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::sources::{RunOutcome, Source};

pub const DEFAULT_POLL_SECONDS: i32 = 300;

/// The `nightscout` poller.
pub async fn poll_nightscout(_ctx: JobContext, _source: Source) -> Result<RunOutcome, JobError> {
    Err(Error::NotImplemented("nightscout::poll_nightscout").into())
}

pub fn register() {
    ownhealth_core::sources::register_poller("nightscout", |ctx, source| Box::pin(poll_nightscout(ctx, source)));
}
