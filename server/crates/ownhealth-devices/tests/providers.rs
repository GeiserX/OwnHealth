//! Every importer's `wearable_source.provider` has its line in `data/connectors/providers.yaml`, stored
//! and served: these are the person's own files and pushes, so agents read them as before. A provider
//! the file does not name is withheld, so an importer added without a line would hide its data from
//! MCP and the rule pack; this test names it instead.

use ownhealth_core::providers::{Ingest, Mcp, policy};

#[test]
fn every_importer_has_a_served_line_in_the_provider_policy() {
    let importers = [
        ownhealth_ingest_structured::apple_health::PROVIDER,
        ownhealth_devices::hae::PROVIDER,
        ownhealth_devices::gadgetbridge::PROVIDER,
        ownhealth_devices::nightscout::PROVIDER,
        ownhealth_devices::fit::PROVIDER,
        ownhealth_devices::cgm_csv::LIBREVIEW,
        ownhealth_devices::cgm_csv::CLARITY,
    ];
    for p in importers {
        let line = policy().providers.get(p);
        assert!(
            line.is_some_and(|l| l.ingest == Ingest::Allow && l.mcp == Mcp::Served),
            "{p}: {line:?}"
        );
    }
}
