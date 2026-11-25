use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use ownhealth_core::Runtime;

use ownhealth_core::access::SCOPES;
use ownhealth_core::break_glass::CATEGORIES;

use super::{
    Bg, Client, CommitOptions, R, Resp, SrcIn, SrcRef, Uuid, candidate, commit_observation, document_page, json, lab,
    lab_document, record, record_uses, seed_genome, seed_medications, setup,
};

const GET_ROUTES: &[&str] = &[
    "/audit",
    "/audit/chain",
    "/audit/who",
    "/auth/invite/{code}",
    "/auth/oidc/callback",
    "/auth/oidc/login",
    "/auth/passkeys",
    "/auth/state",
    "/brands",
    "/break-glass",
    "/break-glass/requests/{id}",
    "/connectors/callback",
    "/connectors/clients",
    "/connectors/profiles",
    "/documents/{document_id}",
    "/documents/{document_id}/file",
    "/documents/{document_id}/pages/{page_no}/words",
    "/documents/{document_id}/pages/{page}",
    "/genome/carrier-couple",
    "/household",
    "/ics/{file}",
    "/inboxes",
    "/inboxes/health",
    "/inboxes/quarantine",
    "/inboxes/{id}",
    "/inboxes/{id}/health",
    "/inboxes/{id}/runs",
    "/inboxes/{id}/senders",
    "/jobs",
    "/mcp/session-report",
    "/me",
    "/notifications",
    "/oauth/consents",
    "/oauth/requests/{id}",
    "/profiles",
    "/profiles/{p}/break-glass",
    "/profiles/{p}/calculators",
    "/profiles/{p}/calendar-feeds",
    "/profiles/{p}/cgm/summary",
    "/profiles/{p}/dashboard",
    "/profiles/{p}/documents",
    "/profiles/{p}/drug-check",
    "/profiles/{p}/due",
    "/profiles/{p}/family",
    "/profiles/{p}/family-shares",
    "/profiles/{p}/gaps",
    "/profiles/{p}/gaps/{key}/letter.pdf",
    "/profiles/{p}/genome",
    "/profiles/{p}/genome/events",
    "/profiles/{p}/genome/variant",
    "/profiles/{p}/ice",
    "/profiles/{p}/next-draw",
    "/profiles/{p}/next-draw.pdf",
    "/profiles/{p}/observations",
    "/profiles/{p}/pedigree",
    "/profiles/{p}/prevention-answers",
    "/profiles/{p}/records/{kind}",
    "/profiles/{p}/shl",
    "/profiles/{p}/shl/{id}/access",
    "/profiles/{p}/status",
    "/profiles/{p}/timeline",
    "/profiles/{p}/trend",
    "/profiles/{p}/vendor-scores",
    "/profiles/{p}/wearables/summary",
    "/profiles/{p}/wearables/{metric}",
    "/review",
    "/review/{item_id}",
    "/rules",
    "/shl/{id}/file/{ticket}",
    "/shl/{id}/ice",
    "/sources/{src_id}",
    "/tokens",
    "/tokens/{token_id}/usage",
    "/wearables/setup",
];

#[test]
fn emergency_use_sweep_covers_every_registered_get_route() {
    fn find_get_routes(dir: &Path, pattern: &regex::Regex, found: &mut BTreeSet<String>) {
        for entry in dir.read_dir().expect("route source directory") {
            let path = entry.expect("route source entry").path();
            if path.is_dir() {
                find_get_routes(&path, pattern, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = std::fs::read_to_string(path).expect("route source");
                found.extend(pattern.captures_iter(&source).map(|m| m[1].to_string()));
            }
        }
    }

    let pattern = regex::Regex::new(r#"\.route\(\s*"([^"]+)"\s*,\s*get\s*\("#).expect("route pattern");
    let mut routes = BTreeSet::new();
    find_get_routes(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src/routes"),
        &pattern,
        &mut routes,
    );
    assert_eq!(routes, GET_ROUTES.iter().map(|route| route.to_string()).collect());
}

async fn paths(b: &mut Bg, sam: &mut Client) -> anyhow::Result<Vec<(String, &'static str)>> {
    seed_genome(b).await?;
    seed_medications(b).await?;
    record(
        b,
        "visit",
        json!({
            "profile_id": b.alex_id(), "period_start": "2025-11-10T09:30:00Z",
            "reason_text": "Synthetic visit", "provider_name": "LabNorte",
        }),
    )
    .await?;

    let profile = b.alex_id();
    let doc = lab_document(b).await?;
    document_page(b, doc).await?;
    let item = candidate(b, Some(doc)).await?;

    let mut tx = b.env.pool().begin().await?;
    let mut citation = SrcIn::new("document_region");
    citation.profile_id = Some(profile);
    citation.document_id = Some(doc);
    citation.page_no = Some(1);
    citation.bbox = Some([10.0, 20.0, 30.0, 40.0]);
    citation.actor_kind = "system".into();
    let plain = commit_observation(
        &mut tx,
        &lab(profile, "97", false)?,
        &SrcRef::New(citation),
        CommitOptions::new("import"),
    )
    .await?;
    sqlx::query("insert into job(kind, state, payload, result) values ('import.fit', 'done', $1, $2)")
        .bind(json!({"profile_id": profile}))
        .bind(json!({"samples": 2}))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let gaps = b.alex.get("/api/profiles/alex/gaps").await?;
    assert_eq!(gaps.status, 200, "{:?}", gaps.json);
    let key = gaps.json["items"][0]["key"].as_str().unwrap_or("unavailable");
    let grant = SCOPES
        .iter()
        .copied()
        .filter(|scope| !scope.starts_with("write:"))
        .collect::<Vec<_>>();
    b.grant_sam("guardian", &grant).await?;
    let feed = sam.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(feed.status, 201, "{:?}", feed.json);
    let file = feed.json["path"]
        .as_str()
        .expect("calendar feed path")
        .trim_start_matches("/api/ics/");

    let unavailable = Uuid::nil().to_string();
    let doc = doc.to_string();
    let item = item.to_string();
    let mut paths = Vec::new();
    for &route in GET_ROUTES {
        let path = route
            .replace("{p}", "alex")
            .replace("{document_id}", &doc)
            .replace("{item_id}", &item)
            .replace("{src_id}", &plain.src_id)
            .replace("{key}", key)
            .replace("{page_no}", "1")
            .replace("{page}", "1.png")
            .replace("{kind}", "medication")
            .replace("{metric}", "resting_hr")
            .replace("{file}", file)
            .replace("{id}", &unavailable)
            .replace("{token_id}", &unavailable)
            .replace("{ticket}", "unavailable")
            .replace("{code}", "unavailable");
        let query = match route {
            "/audit" => "?profile=alex",
            "/audit/who" => "?profile=alex",
            "/genome/carrier-couple" => "?a=alex&b=sam",
            "/profiles/{p}/cgm/summary" => "?end=2026-11-23",
            "/profiles/{p}/documents" => "?q=sampleword",
            "/profiles/{p}/drug-check" => "?drug=atorvastatin",
            "/profiles/{p}/genome/variant" => "?rsid=rs4244285",
            "/profiles/{p}/observations" => "?category=laboratory",
            "/profiles/{p}/trend" => "?code=ldl_c",
            "/review" => "?profile=alex",
            "/sources/{src_id}" => "?include_text=true",
            _ => "",
        };
        paths.push((
            format!("/api/{path}{query}", path = path.trim_start_matches('/')),
            route,
        ));
    }
    paths.push((
        "/api/profiles/alex/observations?category=vital-signs".into(),
        "/profiles/{p}/observations",
    ));
    paths.push((
        "/api/profiles/alex/observations?category=activity".into(),
        "/profiles/{p}/observations",
    ));
    Ok(paths)
}

fn differs(route: &str, grant: &Resp, emergency: &Resp) -> bool {
    if grant.status != emergency.status {
        return true;
    }
    if route == "/ics/{file}" {
        fn events(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
            bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.starts_with(b"DTSTAMP:"))
        }
        return !events(&grant.bytes).eq(events(&emergency.bytes));
    }
    if grant.json.is_null() && emergency.json.is_null() {
        grant.bytes != emergency.bytes
    } else {
        grant.json != emergency.json
    }
}

#[tokio::test]
async fn changed_get_responses_record_use_for_each_added_scope() -> R {
    let mut b = setup().await?;
    let mut sam = b.sam().await?;
    let paths = paths(&mut b, &mut sam).await?;
    // The same session and database, including the active request. The router without the
    // emergency-use recorder only sees ordinary grants, so lifecycle and audit metadata match.
    let mut grant_only = sam.clone();
    let runtime = Runtime {
        settings: Arc::new(b.env.settings.clone()),
        ..b.env.rt.clone()
    };
    grant_only.app = Router::new()
        .nest("/api", ownhealth_api::routes::all())
        .with_state(runtime);
    let mut missing = Vec::new();
    let mut changed = BTreeSet::new();

    for added in CATEGORIES {
        let ordinary: Vec<_> = SCOPES
            .iter()
            .copied()
            .filter(|s| *s != added && !s.starts_with("write:"))
            .collect();
        b.grant_sam("guardian", &ordinary).await?;
        let id = b.open_for(b.env.user("sam"), &mut sam, json!([added])).await?;
        for (path, route) in &paths {
            let grant = grant_only.get(path).await?;
            let before = record_uses(&b, id).await?;
            let r = sam.get(path).await?;
            assert_ne!(r.status, 500, "{added} {route}: {:?}", r.json);
            if r.status.is_success() && differs(route, &grant, &r) {
                changed.insert((added, *route));
                let after = record_uses(&b, id).await?;
                if after != (before.0 + 1, before.1 + 1) {
                    missing.push((added, route));
                }
            }
        }

        let revoked = b
            .alex
            .post(
                &format!("/api/profiles/alex/break-glass/requests/{id}/revoke"),
                &json!({}),
            )
            .await?;
        assert_eq!(revoked.status, 200, "{:?}", revoked.json);
    }

    assert!(changed.contains(&("read:records", "/profiles/{p}/genome")));
    assert!(changed.contains(&("read:documents_text", "/profiles/{p}/documents")));
    assert!(changed.contains(&("read:wearables", "/jobs")));
    assert!(
        missing.is_empty(),
        "Changed successful GET responses without one audit row and one notice: {missing:?}"
    );
    Ok(())
}
