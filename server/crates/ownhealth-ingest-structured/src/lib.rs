//! Lane C: structured imports and records (`ingest/fhir`, `ingest/apple_health`, `ingest/manual.py`,
//! `ingest/retract.py`, `wearables/`; connectors.md 2.2, 4, architecture.md 9). Its demo hook is
//! `demo/hooks/apple_fhir.py` (order 300).

pub mod apple_health;
pub mod brands;
pub mod demo;
pub mod fhir;
pub mod jobs;
pub mod manual;
pub mod py;
pub mod retract;
pub mod shc;
pub mod smart;
pub mod wearables;

use ownhealth_core::jobs::register_handler;

/// Register lane C's job handlers (`import.fhir_bundle`, `import.apple_health`), accept handler
/// (`fhir_unmapped`), the SMART poller (source kind `smart`), demo hook (`apple_fhir`, order 300) and the US
/// endpoint directory poller (`brands`, no default-sources hook).
pub fn register() {
    register_handler("import.fhir_bundle", |ctx, job| {
        Box::pin(jobs::import_fhir_job(ctx, job))
    });
    register_handler("import.apple_health", |ctx, job| {
        Box::pin(jobs::import_apple_health_job(ctx, job))
    });
    ownhealth_core::review::register_accept_handler("fhir_unmapped", fhir::accept_fhir_boxed);
    smart::register();
    ownhealth_core::demo::register_demo_hook("apple_fhir", demo::ORDER, demo::seed_boxed);
    brands::register();
}
