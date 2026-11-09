//! Lane D (v0.2): device data over REST (`ownhealth_devices`).
//!
//! | Route | Auth | What |
//! |---|---|---|
//! | `POST /ingest/health-auto-export` | `Authorization: Bearer oh_ing_...` of a `health_auto_export` source | Health Auto Export JSON v2 -> `{seen, new, duplicate}` |
//! | `GET /profiles/{p}/cgm/summary?days=14&end=` | session, `read:wearables` | the CGM summary (`ownhealth_devices::cgm::CgmSummary`) |
//!
//! The Gadgetbridge export goes through the normal upload (`POST /ingest/upload`, sniffed as
//! `gadgetbridge_db`); a Nightscout site is a `nightscout` source (`/inboxes`).

use axum::Router;
use axum::extract::{Path, State};
use axum::routing::{get, post};
use ownhealth_core::{Error, Runtime};

use crate::error::ApiError;
use crate::{Caller, SourceCaller};

async fn not_yet(_source: SourceCaller) -> ApiError {
    Error::NotImplemented("device routes (lane D)").into()
}

// The access check runs before the 501, as it will in the finished route.
async fn cgm_summary(caller: Caller, State(rt): State<Runtime>, Path(p): Path<String>) -> Result<(), ApiError> {
    let mut conn = rt.pool.acquire().await?;
    caller.require(&mut conn, &p, Some("read:wearables"), None).await?;
    Err(Error::NotImplemented("device routes (lane D)").into())
}

pub fn router() -> Router<Runtime> {
    Router::new()
        .route("/ingest/health-auto-export", post(not_yet))
        .route("/profiles/{p}/cgm/summary", get(cgm_summary))
}
