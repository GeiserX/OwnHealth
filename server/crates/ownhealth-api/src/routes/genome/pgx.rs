//! The PGx gene grid of `GET /api/profiles/{p}/genome` (genomics.md 6, v0.3): one row per CPIC gene
//! with its call or why there is none, and the current medicines from the person's records that CPIC
//! ties to that gene. The medicines need read:records; without it the grid says so (`meds: null`).
//! The grid reads every PGx and HLA call the reader's gate lets through, whatever evidence level the
//! table below it shows: a call under that level is still a call, never "no call in the file".

use std::collections::BTreeMap;

use ownhealth_core::Result;
use ownhealth_core::access::Permission;
use ownhealth_core::audit::AuditEvent;
use ownhealth_core::records::active_medications;
use ownhealth_genome::findings::{genes_for_drug_name, pgx_grid};
use ownhealth_genome::{FindingsQuery, Gate, list_findings_typed};
use ownhealth_rules::engine::{med_drug, str_of};
use ownhealth_rules::load_pack;
use serde_json::{Value, json};
use sqlx::PgConnection;

/// Gene -> the current medicines (name, kind, src) whose CPIC drug that gene guides.
async fn meds_by_gene(conn: &mut PgConnection, perm: &Permission) -> Result<BTreeMap<String, Vec<Value>>> {
    let pack = load_pack()?;
    let meds = active_medications(conn, perm.profile.id, None, None, None, perm.has("read:sensitive")).await?;
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for m in &meds {
        let Some(drug) = med_drug(&pack, m) else { continue };
        // the label the drug check shows: "Atorvastatin 20 mg"
        let name: Vec<&str> = [str_of(m, "name"), str_of(m, "strength")]
            .into_iter()
            .flatten()
            .filter(|x| !x.is_empty())
            .collect();
        for gene in genes_for_drug_name(&drug) {
            out.entry(gene).or_default().push(json!({
                "name": name.join(" "),
                "kind": str_of(m, "kind"),
                "src": str_of(m, "src_id"),
            }));
        }
    }
    Ok(out)
}

/// The grid for one reader, through the same gate as the table.
pub async fn grid(conn: &mut PgConnection, perm: &Permission, gate: &Gate) -> Result<Vec<Value>> {
    let q = FindingsQuery {
        min_evidence: Some("E4"),
        ..FindingsQuery::default()
    };
    let found = list_findings_typed(&mut *conn, perm.profile.id, gate, &q).await?;
    let meds = if perm.has("read:records") {
        Some(meds_by_gene(conn, perm).await?)
    } else {
        None
    };
    Ok(pgx_grid(&found, meds.as_ref()))
}

/// The audit of the overview names what the grid returned too: its calls and its medicines.
pub fn audit_grid(e: &mut AuditEvent, grid: &[Value]) {
    let add = |into: &mut Option<Vec<String>>, ids: Vec<&str>| {
        let list = into.get_or_insert_with(Vec::new);
        for id in ids {
            if !list.iter().any(|x| x == id) {
                list.push(id.to_string());
            }
        }
    };
    let facts = grid.iter().flat_map(|r| r["fact_ids"].as_array().into_iter().flatten());
    add(&mut e.resource_ids, facts.filter_map(Value::as_str).collect());
    let meds = grid.iter().flat_map(|r| r["meds"].as_array().into_iter().flatten());
    let srcs = grid.iter().map(|r| &r["src"]).chain(meds.map(|m| &m["src"]));
    add(&mut e.src_ids, srcs.filter_map(Value::as_str).collect());
}
