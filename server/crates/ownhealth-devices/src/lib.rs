//! Lane D (v0.2): wearables and devices without a vendor cloud (connectors.md 2.2-2.3, 4).
//!
//! | Module | What |
//! |---|---|
//! | `hae` | Health Auto Export (iOS) JSON v2 push to `POST /api/ingest/health-auto-export` with a push token of a `health_auto_export` source; scheduled every 15 min by the app; 0 duplicate samples on re-push |
//! | `gadgetbridge` | the Gadgetbridge SQLite export (`import.gadgetbridge`, sniffed as `gadgetbridge_db` on upload): device tables by device type |
//! | `nightscout` | pull `/api/v1/entries/sgv.json` (and the v3 API) from the person's own Nightscout with a read token; FreeStyle Libre arrives the same way through nightscout-connect |
//! | `cgm` | CGM readings as LOINC 99504-3; the HL7 CGM IG summary (mean glucose 97507-8, GMI, times in ranges 106793-3); valid only with >= 70% sensor wear over 14 days |
//!
//! Samples go to `wearable_sample` through lane C's (0.2.0) wearables store, with a `wearable_source`
//! row linked to the connector `source` (`wearable_source.source_id`). Raw pushes and files are stored
//! as blobs first (connectors.md 6.3 rule 1).

pub mod cgm;
pub mod gadgetbridge;
pub mod hae;
pub mod nightscout;

/// Register `import.gadgetbridge` and the `nightscout` poller. Called by `ownhealth::register_lanes`.
pub fn register() {
    gadgetbridge::register();
    nightscout::register();
}
