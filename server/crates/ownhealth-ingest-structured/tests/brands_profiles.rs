//! The connection profiles (`data/connectors/connectors.yaml`) and the US provider directory name the
//! same vendors: a profile the wizard searches the directory for has a vendor file there, every
//! vendor file has such a profile, and a fixed directory entry is one of its profile's servers.

use ownhealth_core::connector_profiles as profiles;
use ownhealth_ingest_structured::brands::{BUNDLES, SINGLES};

#[test]
fn the_directory_and_the_profiles_name_the_same_vendors() {
    let file = profiles::profiles().expect("connectors.yaml");
    for v in file.vendors.iter().filter(|v| v.directory) {
        assert!(
            BUNDLES.iter().any(|b| b.vendor == v.key),
            "{} is searched in the directory, which has no file of it",
            v.key
        );
    }
    for b in BUNDLES {
        assert!(
            file.get(b.vendor).is_some_and(|v| v.directory),
            "the directory's {} entries have no profile that searches them",
            b.vendor
        );
    }
    for (vendor, _, endpoint) in SINGLES {
        let v = file.get(vendor).unwrap_or_else(|| panic!("{vendor} has no profile"));
        assert!(
            v.servers.iter().any(|s| s.trim_end_matches('/') == endpoint.trim_end_matches('/')),
            "{vendor}: the directory's {endpoint} is not among {:?}",
            v.servers
        );
    }
}
