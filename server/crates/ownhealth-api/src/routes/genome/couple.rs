//! The couple carrier view (genomics.md section 10), lane D.
//!
//! ```text
//! GET /api/genome/carrier-couple?a=&b=&lang=    read:genome on both profiles
//! ```
//!
//! Both profiles need read:genome for the caller (a lock on read:genome removes it), and both must be
//! adult and living: a minor or a deceased profile is refused before any fact is read. Each side goes
//! through its own gate (`gate_for`), and the join (`ownhealth_genome::couple`) names a sensitive gene
//! only when both gates open it. Each profile gets one audit row with identifiers only.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use ownhealth_core::Runtime;
use ownhealth_core::access::{Permission, Principal, PrincipalKind, majority, today};
use ownhealth_core::audit::{AuditEvent, write_audit};
use ownhealth_genome::{NOTICE, couple, gate_for};
use serde_json::{Value, json};
use std::collections::HashMap;

use super::super::auth::{R, detail, ok};
use crate::extract::Caller;

const TOOL: &str = "GET /api/genome/carrier-couple";

fn surface(p: &Principal) -> &'static str {
    if p.kind == PrincipalKind::Token { "agents" } else { "ui" }
}

fn event(
    p: &Principal,
    perm: &Permission,
    other: &Permission,
    used: &(Vec<String>, Vec<String>),
    withheld: usize,
) -> AuditEvent {
    let mut e = AuditEvent::new(
        p.actor_kind(),
        if p.kind == PrincipalKind::Token { "api" } else { "ui" },
        "read",
    );
    e.actor_id = p.user_id;
    e.token_id = p.token_id;
    e.client = p.token_label.clone();
    e.tool = Some(TOOL.into());
    e.profile_id = Some(perm.profile.id);
    e.args_summary = Some(format!("with={}", other.profile.id));
    e.resource_type = Some("genome_finding".into());
    e.resource_ids = Some(used.0.clone());
    e.src_ids = Some(used.1.clone());
    e.rows_out = i32::try_from(used.0.len()).ok();
    e.withheld = Some(json!({"sensitive": withheld}));
    e
}

/// `GET /genome/carrier-couple` for one principal.
pub async fn carrier_couple(rt: &Runtime, principal: &Principal, a: &str, b: &str, lang: &str) -> R<Value> {
    if a.is_empty() || b.is_empty() {
        return Err(detail(
            StatusCode::BAD_REQUEST,
            "bad_input",
            "Name two profiles: a and b.",
        ));
    }
    let mut tx = rt.pool.begin().await?;
    let caller = Caller(principal.clone());
    let pa = caller.require(&mut tx, a, Some("read:genome"), None).await?;
    let pb = caller.require(&mut tx, b, Some("read:genome"), None).await?;
    if pa.profile.id == pb.profile.id {
        return Err(detail(
            StatusCode::BAD_REQUEST,
            "bad_input",
            "Name two different profiles.",
        ));
    }
    let on = today();
    let mut gates = Vec::new();
    for perm in [&pa, &pb] {
        let aom = majority(&mut tx, perm.profile.household_id).await?;
        let gate = gate_for(perm, surface(principal), on, aom)?;
        if gate.minor || perm.profile.deceased {
            return Err(detail(
                StatusCode::BAD_REQUEST,
                "couple_not_eligible",
                "The couple carrier view joins two adult, living profiles.",
            ));
        }
        gates.push(gate);
    }
    let person_a = couple::load_person(&mut tx, pa.profile.id, &pa.profile.display_name).await?;
    let person_b = couple::load_person(&mut tx, pb.profile.id, &pb.profile.display_name).await?;
    let joined = couple::join(&person_a, &person_b, &gates[0], &gates[1], lang)?;
    write_audit(&mut tx, &event(principal, &pa, &pb, &joined.a_used, joined.withheld)).await?;
    write_audit(&mut tx, &event(principal, &pb, &pa, &joined.b_used, joined.withheld)).await?;
    tx.commit().await?;
    let who = |p: &Permission| json!({"profile": p.profile.slug, "name": p.profile.display_name});
    Ok(json!({
        "a": who(&pa),
        "b": who(&pb),
        "lang": if lang == "es" { "es" } else { "en" },
        "rules": couple::rules_version(),
        "rows": joined.rows,
        "withheld": {"sensitive": joined.withheld},
        "counsel": couple::text("counsel", lang, &[]),
        "notice": NOTICE,
    }))
}

async fn carrier_couple_route(
    State(rt): State<Runtime>,
    Caller(c): Caller,
    Query(q): Query<HashMap<String, String>>,
) -> R {
    let arg = |k: &str| q.get(k).map_or("", String::as_str);
    ok(carrier_couple(&rt, &c, arg("a"), arg("b"), arg("lang")).await?)
}

pub fn router() -> Router<Runtime> {
    Router::new().route("/genome/carrier-couple", get(carrier_couple_route))
}
