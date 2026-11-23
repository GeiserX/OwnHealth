//! The PGx gene grid of `GET /api/profiles/{p}/genome` (genomics.md 6, v0.3): one row per CPIC gene
//! with its call or why there is none, and the current medicines from the person's records that CPIC
//! ties to that gene. The medicines need read:records; without it the grid says so (`meds: null`).
//! The grid reads every PGx and HLA call the reader's gate lets through down to E3, or down to E4 when
//! the reader asked for E4, whatever stricter level the table below it shows. E4 and hidden-by-default
//! calls stay hidden until asked for (genomics.md 3); a CPIC gene whose only call is hidden or under
//! the level reads `withheld`, never "no call in the file".

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
    let meds = active_medications(
        conn,
        perm.profile.id,
        None,
        None,
        None,
        perm.may_read_sensitive("read:records"),
    )
    .await?;
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

/// The grid of one reader, and the src of every call it read (a gene's row shows one call's src).
pub struct Grid {
    pub rows: Vec<Value>,
    call_srcs: Vec<String>,
}

impl Grid {
    /// The PGx genes the grid frames as actionable: the PGx tile counts these, so the two agree.
    pub fn actionable_pgx(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r["actionable"] == true && !r["gene"].as_str().unwrap_or_default().starts_with("HLA-"))
            .count()
    }
}

/// The grid for one reader, through the same gate as the table. `min_evidence` is the table's level:
/// the grid reads at that level or E3, whichever lets more through.
pub async fn grid(conn: &mut PgConnection, perm: &Permission, gate: &Gate, min_evidence: &str) -> Result<Grid> {
    let q = FindingsQuery {
        min_evidence: Some(min_evidence.max("E3")),
        ..FindingsQuery::default()
    };
    let found = list_findings_typed(&mut *conn, perm.profile.id, gate, &q).await?;
    let meds = if perm.has("read:records") {
        perm.record_read(&["read:records"]);
        Some(meds_by_gene(conn, perm).await?)
    } else {
        None
    };
    let call_srcs = found
        .findings
        .iter()
        .filter(|f| f["category"] == "pgx" || f["category"] == "hla")
        .filter_map(|f| f["src"].as_str().map(str::to_string))
        .collect();
    Ok(Grid {
        rows: pgx_grid(&found, meds.as_ref()),
        call_srcs,
    })
}

/// The audit of the overview names what the grid returned too: every call it read and its medicines.
pub fn audit_grid(e: &mut AuditEvent, grid: &Grid) {
    let add = |into: &mut Option<Vec<String>>, ids: Vec<&str>| {
        let list = into.get_or_insert_with(Vec::new);
        for id in ids {
            if !list.iter().any(|x| x == id) {
                list.push(id.to_string());
            }
        }
    };
    let facts = grid
        .rows
        .iter()
        .flat_map(|r| r["fact_ids"].as_array().into_iter().flatten());
    add(&mut e.resource_ids, facts.filter_map(Value::as_str).collect());
    let meds = grid
        .rows
        .iter()
        .flat_map(|r| r["meds"].as_array().into_iter().flatten());
    let srcs = grid.call_srcs.iter().map(String::as_str);
    add(
        &mut e.src_ids,
        srcs.chain(meds.filter_map(|m| m["src"].as_str())).collect(),
    );
}
