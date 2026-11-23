//! Break-glass, server side (migration 1260): a contact the owner named asks for emergency access, the
//! owner's calendar feed carries the request at once, and access opens once the delay has passed and
//! that feed has served the request. Each test runs on its own throwaway database as the synthetic
//! Rivera family: Alex owns `alex`, Sam holds a viewer grant on it, and Carmen Ortega (a login of the
//! household made here) holds no grant on it.

#[path = "auth_support.rs"]
mod support;

use chrono::{DateTime, Duration, Utc};
use icalendar::{Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime};
use ownhealth_core::access::{create_user, set_grant};
use ownhealth_core::break_glass::settle;
use serde_json::{Value, json};
use support::{Client, Env, Resp};
use uuid::Uuid;

type R = anyhow::Result<()>;

struct Bg {
    env: Env,
    alex: Client,
    feed: String,
    carmen_id: Uuid,
}

/// The family, Carmen's login, and a calendar feed Alex made on his own profile.
async fn setup() -> anyhow::Result<Bg> {
    let mut env = Env::new(true).await?;
    // sharing on, so a share link is refused for who asks, not because the instance shares nothing
    env.settings.share_url = Some("https://share.rivera.example".into());
    let carmen_id = {
        let mut c = env.pool().acquire().await?;
        let id = create_user(&mut c, env.fam().household_id, "Carmen Ortega", "es", None).await?;
        set_grant(&mut c, id, env.profile("carmen"), "owner", None, None, None).await?;
        id
    };
    let mut alex = env.as_user(env.user("alex")).await?;
    let made = alex.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(made.status, 201, "{:?}", made.json);
    let feed = made.json["path"].as_str().unwrap_or_default().to_string();
    Ok(Bg {
        env,
        alex,
        feed,
        carmen_id,
    })
}

impl Bg {
    fn alex_id(&self) -> Uuid {
        self.env.profile("alex")
    }

    async fn carmen(&self) -> anyhow::Result<Client> {
        self.env.as_user(self.carmen_id).await
    }

    async fn sam(&self) -> anyhow::Result<Client> {
        self.env.as_user(self.env.user("sam")).await
    }

    /// Alex names `user` with these settings.
    async fn name(&mut self, user: Uuid, body: Value) -> anyhow::Result<Resp> {
        self.alex
            .put(&format!("/api/profiles/alex/break-glass/contacts/{user}"), &body)
            .await
    }

    /// `who` asks for access to Alex's profile; the new request's id.
    async fn ask(&self, who: &mut Client) -> anyhow::Result<Uuid> {
        let r = who
            .post(
                "/api/break-glass/requests",
                &json!({"profile": self.alex_id(), "reason": "En el hospital"}),
            )
            .await?;
        assert_eq!(r.status, 201, "{:?}", r.json);
        Ok(r.json["id"].as_str().unwrap_or_default().parse()?)
    }

    /// A calendar app fetches Alex's feed.
    async fn fetch_feed(&self) -> anyhow::Result<String> {
        let r = self.env.anon().get(&self.feed).await?;
        assert_eq!(r.status, 200);
        Ok(String::from_utf8(r.bytes)?)
    }

    async fn request(
        &self,
        id: Uuid,
    ) -> anyhow::Result<(String, DateTime<Utc>, Option<DateTime<Utc>>, Option<DateTime<Utc>>)> {
        Ok(sqlx::query_as(
            "select status, activates_at, activated_at, expires_at from break_glass_request where id = $1",
        )
        .bind(id)
        .fetch_one(self.env.pool())
        .await?)
    }

    async fn settle_at(&self, at: DateTime<Utc>) -> anyhow::Result<ownhealth_core::break_glass::Settled> {
        let mut c = self.env.pool().acquire().await?;
        Ok(settle(&mut c, Some(at)).await?)
    }

    /// Whether `who` can read Alex's status page now.
    async fn reads(&self, who: &mut Client) -> anyhow::Result<bool> {
        let r = who.get("/api/profiles/alex/status").await?;
        assert!(r.status == 200 || r.status == 404, "{} {:?}", r.status, r.json);
        Ok(r.status == 200)
    }

    async fn count(&self, sql: &str, id: Uuid) -> anyhow::Result<i64> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .fetch_one(self.env.pool())
            .await?)
    }
}

/// `/api/me`'s entry for `alex`, or Null.
async fn me_alex(who: &mut Client) -> anyhow::Result<Value> {
    let me = who.get("/api/me").await?;
    assert_eq!(me.status, 200, "{:?}", me.json);
    Ok(me.json["profiles"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|p| p["slug"] == "alex")
        .cloned()
        .unwrap_or(Value::Null))
}

#[tokio::test]
async fn naming_a_contact_needs_an_owner_calendar_feed_and_step_up() -> R {
    let mut b = setup().await?;
    let carmen = b.carmen_id;
    // the feed is the owner's only channel: with it revoked, naming is refused
    let feeds = b.alex.get("/api/profiles/alex/calendar-feeds").await?;
    let feed_id = feeds.json["items"][0]["id"].as_str().unwrap_or_default().to_string();
    let gone = b
        .alex
        .delete(&format!("/api/profiles/alex/calendar-feeds/{feed_id}"))
        .await?;
    assert_eq!(gone.status, 200);
    let r = b.name(carmen, json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (409, "no_calendar_feed"), "{:?}", r.json);
    let made = b.alex.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(made.status, 201);
    // never sensitive, raw genome or a write; the delay is 12 to 168 hours
    let r = b
        .name(carmen, json!({"categories": ["read:summary", "read:sensitive"]}))
        .await?;
    assert_eq!(r.status, 422, "{:?}", r.json);
    let r = b.name(carmen, json!({"delay_hours": 6})).await?;
    assert_eq!(r.status, 422, "{:?}", r.json);
    // a stale session cannot widen access
    let mut stale = b.env.stale(b.env.user("alex")).await?;
    let r = stale
        .put(&format!("/api/profiles/alex/break-glass/contacts/{carmen}"), &json!({}))
        .await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "step_up_required"), "{:?}", r.json);
    let r = b.name(carmen, json!({})).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(
        r.json["categories"],
        json!(["read:labs", "read:records", "read:summary"])
    );
    assert_eq!(r.json["delay_hours"], 48);
    // only the owner names contacts
    let mut sam = b.sam().await?;
    let r = sam
        .put(&format!("/api/profiles/alex/break-glass/contacts/{carmen}"), &json!({}))
        .await?;
    assert_eq!(r.status, 403, "{:?}", r.json);
    Ok(())
}

/// The request opens at its delay (±1 s under an injected clock) once the owner feed has served it,
/// for 14 days; until then the contact reads nothing.
#[tokio::test]
async fn a_request_opens_at_its_delay_once_the_owner_feed_was_fetched() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    assert!(!b.reads(&mut carmen).await?);
    b.fetch_feed().await?;
    let (status, opens, ..) = b.request(id).await?;
    assert_eq!(status, "requested");
    let before = b.settle_at(opens - Duration::seconds(1)).await?;
    assert!(before.activated.is_empty());
    assert_eq!(b.request(id).await?.0, "requested");
    let after = b.settle_at(opens + Duration::seconds(1)).await?;
    assert_eq!(after.activated, vec![id]);
    let (status, _, activated, expires) = b.request(id).await?;
    assert_eq!(status, "active");
    assert_eq!(activated, Some(opens + Duration::seconds(1)));
    assert_eq!(expires, Some(opens + Duration::seconds(1) + Duration::days(14)));
    assert!(b.reads(&mut carmen).await?);
    // read-only: no import, review, write, grants, export or delete
    let me = me_alex(&mut carmen).await?;
    assert_eq!(me["role"], "viewer");
    assert_eq!(me["break_glass"], true);
    assert_eq!(me["scopes"], json!(["read:labs", "read:records", "read:summary"]));
    Ok(())
}

/// Past its delay with no feed fetch since the request it stays requested, and says why; a fetch
/// after the delay opens it at the contact's next request, or at the sweep.
#[tokio::test]
async fn without_a_feed_fetch_a_request_waits_and_a_later_fetch_opens_it() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({"delay_hours": 12})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    // the delay has passed (moved back here, as the clock would)
    sqlx::query(
        "update break_glass_request set requested_at = now() - interval '13 hours', \
         activates_at = now() - interval '1 hour' where id = $1",
    )
    .bind(id)
    .execute(b.env.pool())
    .await?;
    assert!(b.settle_at(Utc::now() + Duration::days(3)).await?.activated.is_empty());
    assert!(!b.reads(&mut carmen).await?);
    assert_eq!(b.request(id).await?.0, "requested");
    let view = b.alex.get("/api/profiles/alex/break-glass").await?;
    assert_eq!(
        view.json["requests"][0]["waiting_for_calendar"], true,
        "{:?}",
        view.json
    );
    let mine = carmen.get("/api/break-glass").await?;
    assert_eq!(
        mine.json["items"][0]["request"]["waiting_for_calendar"], true,
        "{:?}",
        mine.json
    );
    b.fetch_feed().await?;
    // the contact's next request opens it
    assert!(b.reads(&mut carmen).await?);
    assert_eq!(b.request(id).await?.0, "active");
    let view = b.alex.get("/api/profiles/alex/break-glass").await?;
    assert_eq!(view.json["requests"][0]["waiting_for_calendar"], false);

    // the sweep opens one too, with nobody coming back
    let mut sam = b.sam().await?;
    b.name(b.env.user("sam"), json!({"delay_hours": 12})).await?;
    let second = b.ask(&mut sam).await?;
    sqlx::query(
        "update break_glass_request set requested_at = now() - interval '13 hours', \
         activates_at = now() - interval '1 hour' where id = $1",
    )
    .bind(second)
    .execute(b.env.pool())
    .await?;
    b.fetch_feed().await?;
    let mut c = b.env.pool().acquire().await?;
    assert_eq!(settle(&mut c, None).await?.activated, vec![second]);
    Ok(())
}

#[tokio::test]
async fn a_cancelled_request_never_activates() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    // one open request at a time
    let again = carmen
        .post(
            "/api/break-glass/requests",
            &json!({"profile": b.alex_id(), "reason": "otra vez"}),
        )
        .await?;
    assert_eq!(again.status, 409, "{:?}", again.json);
    b.fetch_feed().await?;
    // the contact cannot cancel or revoke through the owner's routes
    let r = carmen
        .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/cancel"))
        .await?;
    assert_eq!(r.status, 404, "{:?}", r.json);
    let r = b
        .alex
        .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/cancel"))
        .await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["status"], "cancelled");
    let (_, opens, ..) = b.request(id).await?;
    assert!(b.settle_at(opens + Duration::days(1)).await?.activated.is_empty());
    assert_eq!(b.request(id).await?.0, "cancelled");
    assert!(!b.reads(&mut carmen).await?);
    // it cannot be revoked or cancelled twice, and the feed no longer shows it
    for step in ["cancel", "revoke"] {
        let r = b
            .alex
            .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/{step}"))
            .await?;
        assert_eq!(r.status, 409, "{step} {:?}", r.json);
    }
    assert!(!b.fetch_feed().await?.contains(&format!("break-glass/{id}")));
    // a database row cannot be moved from cancelled to active either
    let forced = sqlx::query(
        "update break_glass_request set status = 'active', activated_at = now(), \
         expires_at = now() + interval '1 day' where id = $1",
    )
    .bind(id)
    .execute(b.env.pool())
    .await;
    assert!(forced.is_err(), "a cancelled request was moved to active");
    // a new request may follow
    b.ask(&mut carmen).await?;
    Ok(())
}

/// Sam's own viewer grant (read:summary) is the same after the request ends, by revoke and by expiry.
#[tokio::test]
async fn revoke_and_expiry_give_the_contact_their_own_grant_back_unchanged() -> R {
    let mut b = setup().await?;
    let sam_id = b.env.user("sam");
    let grant_sql = "select role, categories, break_glass, expires_at, granted_at, granted_by from profile_grant \
                     where user_id = $1 and profile_id = $2";
    type Grant = (
        String,
        Vec<String>,
        bool,
        Option<DateTime<Utc>>,
        DateTime<Utc>,
        Option<Uuid>,
    );
    let (pool, alex_profile) = (b.env.pool().clone(), b.env.profile("alex"));
    let grant = || {
        let pool = pool.clone();
        async move {
            let g: Grant = sqlx::query_as(grant_sql)
                .bind(sam_id)
                .bind(alex_profile)
                .fetch_one(&pool)
                .await?;
            anyhow::Ok(g)
        }
    };
    let original = grant().await?;
    let mut sam = b.sam().await?;
    let own = me_alex(&mut sam).await?;
    assert_eq!(
        (own["role"].clone(), own["scopes"].clone(), own["break_glass"].clone()),
        (json!("viewer"), json!(["read:summary"]), json!(false))
    );
    b.name(
        sam_id,
        json!({"categories": ["read:summary", "read:labs", "read:records"]}),
    )
    .await?;

    for ending in ["revoke", "expiry"] {
        let id = b.ask(&mut sam).await?;
        b.fetch_feed().await?;
        let (_, opens, ..) = b.request(id).await?;
        b.settle_at(opens).await?;
        let open = me_alex(&mut sam).await?;
        assert_eq!(
            open["scopes"],
            json!(["read:labs", "read:records", "read:summary"]),
            "{ending}"
        );
        assert_eq!(open["break_glass"], true);
        assert_eq!(
            grant().await?,
            original,
            "{ending}: the grant row moved while the request was open"
        );
        if ending == "revoke" {
            let r = b
                .alex
                .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/revoke"))
                .await?;
            assert_eq!(r.status, 200, "{:?}", r.json);
            assert_eq!(r.json["status"], "revoked");
        } else {
            let (.., expires) = b.request(id).await?;
            let expires = expires.unwrap_or_default();
            assert!(b.settle_at(expires - Duration::seconds(1)).await?.expired.is_empty());
            assert_eq!(b.settle_at(expires + Duration::seconds(1)).await?.expired, vec![id]);
            assert_eq!(b.request(id).await?.0, "expired");
        }
        let back = me_alex(&mut sam).await?;
        assert_eq!(back, own, "{ending}");
        assert_eq!(grant().await?, original, "{ending}");
    }
    Ok(())
}

/// What only break-glass reaches is never handed on: no token, consent, export, share link or calendar
/// feed; a token minted during the grant cannot read the profile, and a bearer token is refused on
/// REST. /api/me, the token list and export agree.
#[tokio::test]
async fn tokens_consents_export_links_and_feeds_refuse_what_only_break_glass_reaches() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(id).await?;
    b.settle_at(opens).await?;
    assert!(b.reads(&mut carmen).await?);
    let me = me_alex(&mut carmen).await?;
    assert_eq!(me["break_glass"], true, "{me}");

    let token = |profiles: Value| json!({"label": "Claude", "profiles": profiles, "scopes": ["read:summary"]});
    let r = carmen.post("/api/tokens", &token(json!(["alex"]))).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let r = carmen.post("/api/tokens", &token(json!(["carmen", "alex"]))).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let mut c = b.env.pool().acquire().await?;
    let consent = ownhealth_auth::oauth::ConsentIn {
        client_id: "https://app.example/oauth/client.json".into(),
        scopes: vec!["read:summary".into()],
        profile_ids: vec![b.alex_id()],
        default_profile: None,
        identity_mode: "full".into(),
        days: None,
    };
    let refused = ownhealth_auth::oauth::approve(&mut c, b.carmen_id, &consent).await;
    assert_eq!(
        refused
            .err()
            .and_then(|e| e.access_code().map(str::to_string))
            .as_deref(),
        Some("break_glass")
    );
    let r = carmen.post("/api/profiles/alex/export", &json!({"fmt": "zip"})).await?;
    assert_eq!(r.status, 403, "{:?}", r.json);
    // the IPS Bundle is an export too: a summary reader gets it on their own grant, never through
    // break-glass
    let r = carmen.post("/api/profiles/alex/export", &json!({"fmt": "ips"})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let r = carmen.post("/api/profiles/alex/shl", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "not_sharer"), "{:?}", r.json);
    let r = carmen.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let r = carmen.get("/api/profiles/alex/break-glass").await?;
    assert_eq!(r.status, 403, "{:?}", r.json);

    // a token minted during the grant, for Carmen's own profile: it lists only that one and cannot read
    // Alex's, over MCP or REST
    let made = carmen.post("/api/tokens", &token(json!(["carmen"]))).await?;
    assert_eq!(made.status, 201, "{:?}", made.json);
    assert_eq!(made.json["token"]["profiles"], json!(["carmen"]));
    let listed = carmen.get("/api/tokens").await?;
    assert!(
        listed
            .json
            .as_array()
            .into_iter()
            .flatten()
            .chain(listed.json["items"].as_array().into_iter().flatten())
            .all(|t| !t["profiles"].as_array().into_iter().flatten().any(|p| p == "alex")),
        "{:?}",
        listed.json
    );
    let plaintext = made.json["plaintext"].as_str().unwrap_or_default().to_string();
    let call = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "get_health_status", "arguments": {"profile": "alex"},
                   "_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28",
                             "io.modelcontextprotocol/clientCapabilities": {}}}});
    let req = http::Request::post("/mcp")
        .header("host", "127.0.0.1:8080")
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::ACCEPT, "application/json, text/event-stream")
        .header(http::header::AUTHORIZATION, format!("Bearer {plaintext}"))
        .header("mcp-protocol-version", "2026-07-28")
        .header("mcp-method", "tools/call")
        .header("mcp-name", "get_health_status")
        .body(axum::body::Body::from(call.to_string()))?;
    let res = tower::ServiceExt::oneshot(ownhealth_mcp::router(b.env.rt.clone()), req).await?;
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await?;
    let answer: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let mut bearer = b.env.anon();
    let r = bearer
        .send(
            http::Method::GET,
            "/api/profiles/alex/status",
            None,
            &[("authorization", &format!("Bearer {plaintext}"))],
        )
        .await?;
    assert_eq!(r.status, 403, "{:?}", r.json);

    // revoked: /api/me drops the profile, and nothing reads it
    let r = b
        .alex
        .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/revoke"))
        .await?;
    assert_eq!(r.status, 200);
    assert_eq!(me_alex(&mut carmen).await?, Value::Null);
    assert!(!b.reads(&mut carmen).await?);
    Ok(())
}

#[tokio::test]
async fn the_owner_feed_shows_the_request_at_once() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    let (requested, opens): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("select requested_at, activates_at from break_glass_request where id = $1")
            .bind(id)
            .fetch_one(b.env.pool())
            .await?;
    let text = b.fetch_feed().await?;
    let cal: Calendar = text.parse().map_err(anyhow::Error::msg)?;
    let events: Vec<_> = cal
        .components
        .iter()
        .filter_map(|c| match c {
            CalendarComponent::Event(e) => Some(e),
            _ => None,
        })
        .filter(|e| e.get_summary() == Some("OwnHealth: solicitud de acceso de emergencia"))
        .collect();
    assert_eq!(events.len(), 1, "{text}");
    let e = events[0];
    let secs = |t: DateTime<Utc>| t.timestamp();
    let start = match e.get_start() {
        Some(DatePerhapsTime::DateTime(CalendarDateTime::Utc(t))) => Some(secs(t)),
        _ => None,
    };
    let end = match e.get_end() {
        Some(DatePerhapsTime::DateTime(CalendarDateTime::Utc(t))) => Some(secs(t)),
        _ => None,
    };
    assert_eq!((start, end), (Some(secs(requested)), Some(secs(opens))));
    let description = e.get_description().unwrap_or_default();
    assert!(
        description.contains(&opens.format("%Y-%m-%d %H:%M UTC").to_string()),
        "{description}"
    );
    assert!(
        description.contains(&format!("https://testserver/family/break-glass/{id}")),
        "{description}"
    );
    // a generic feed names nobody
    assert!(!description.contains("Carmen"), "{description}");
    let fed: Option<DateTime<Utc>> = sqlx::query_scalar("select owner_feed_at from break_glass_request where id = $1")
        .bind(id)
        .fetch_one(b.env.pool())
        .await?;
    assert!(fed.is_some());
    // a feed of someone who does not own the profile neither shows nor stamps a request
    let mut sam = b.sam().await?;
    let lucia = sam.post("/api/profiles/lucia/calendar-feeds", &json!({})).await?;
    assert_eq!(lucia.status, 201);
    let other = b
        .env
        .anon()
        .get(lucia.json["path"].as_str().unwrap_or_default())
        .await?;
    assert!(!String::from_utf8(other.bytes)?.contains("break-glass"));
    Ok(())
}

/// Request, cancel, activation, each use, expiry and revoke: each writes its audit row and an outbox
/// row for the owner, and the owner reads and clears the outbox.
#[tokio::test]
async fn every_step_writes_an_audit_row_and_an_outbox_row() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let alex = b.env.user("alex");
    let audit = "select count(*) from audit_event where action = $2 and resource_ids = array[$1::text]";
    let outbox = "select count(*) from notification_outbox where request_id = $1 and kind = $2 and user_id = $3";
    let rows = |id: Uuid, step: &'static str| {
        let pool = b.env.pool().clone();
        async move {
            let a: i64 = sqlx::query_scalar(audit)
                .bind(id.to_string())
                .bind(step)
                .fetch_one(&pool)
                .await?;
            let o: i64 = sqlx::query_scalar(outbox)
                .bind(id)
                .bind(step)
                .bind(alex)
                .fetch_one(&pool)
                .await?;
            anyhow::Ok((a, o))
        }
    };

    let first = b.ask(&mut carmen).await?;
    assert_eq!(rows(first, "break_glass.request").await?, (1, 1));
    b.alex
        .post0(&format!("/api/profiles/alex/break-glass/requests/{first}/cancel"))
        .await?;
    assert_eq!(rows(first, "break_glass.cancel").await?, (1, 1));

    let second = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(second).await?;
    b.settle_at(opens).await?;
    assert_eq!(rows(second, "break_glass.activate").await?, (1, 1));
    // one use per request the contact makes through it, however many checks the route runs
    assert_eq!(rows(second, "break_glass.use").await?, (0, 0));
    assert!(b.reads(&mut carmen).await?);
    assert_eq!(rows(second, "break_glass.use").await?, (1, 1));
    // listing the profile (with its open review count) is a use too
    assert_ne!(me_alex(&mut carmen).await?, Value::Null);
    assert_eq!(rows(second, "break_glass.use").await?, (2, 2));
    // a request of Carmen's that reads only her own profile, or her own requests, is not
    let own = carmen.get("/api/profiles/carmen/status").await?;
    assert_eq!(own.status, 200, "{:?}", own.json);
    let r = carmen.get("/api/break-glass").await?;
    assert_eq!(r.status, 200);
    assert_eq!(rows(second, "break_glass.use").await?, (2, 2));
    b.alex
        .post0(&format!("/api/profiles/alex/break-glass/requests/{second}/revoke"))
        .await?;
    assert_eq!(rows(second, "break_glass.revoke").await?, (1, 1));

    let third = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(third).await?;
    b.settle_at(opens).await?;
    let (.., expires) = b.request(third).await?;
    b.settle_at(expires.unwrap_or_default()).await?;
    assert_eq!(rows(third, "break_glass.expire").await?, (1, 1));
    // the audit rows name the request, never the reason
    let leaked = b
        .count(
            "select count(*) from audit_event where args_summary like '%hospital%' or tool like '%hospital%' or $1::uuid is null",
            third,
        )
        .await?;
    assert_eq!(leaked, 0);
    let mut c = b.env.pool().acquire().await?;
    assert!(ownhealth_core::audit::verify_chain(&mut c).await?.ok());

    // the owner reads the outbox and clears it; the contact has none
    let n = b.alex.get("/api/notifications").await?;
    assert_eq!(n.status, 200);
    let kinds: std::collections::BTreeSet<&str> = n.json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["kind"].as_str())
        .collect();
    assert_eq!(
        kinds,
        ownhealth_core::break_glass::OUTBOX_KINDS.iter().copied().collect()
    );
    assert!(n.json["unread"].as_i64().unwrap_or_default() > 0);
    let read = b.alex.post("/api/notifications/read", &json!({})).await?;
    assert_eq!(read.status, 200, "{:?}", read.json);
    assert_eq!(b.alex.get("/api/notifications").await?.json["unread"], 0);
    assert_eq!(carmen.get("/api/notifications").await?.json["items"], json!([]));
    Ok(())
}

/// Removing a contact ends their open request, and only an owner of the profile hears of it.
#[tokio::test]
async fn removing_a_contact_ends_their_open_request() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(id).await?;
    b.settle_at(opens).await?;
    assert!(b.reads(&mut carmen).await?);
    let r = b
        .alex
        .delete(&format!("/api/profiles/alex/break-glass/contacts/{}", b.carmen_id))
        .await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(b.request(id).await?.0, "revoked");
    assert!(!b.reads(&mut carmen).await?);
    let again = carmen
        .post(
            "/api/break-glass/requests",
            &json!({"profile": b.alex_id(), "reason": "otra vez"}),
        )
        .await?;
    assert_eq!((again.status.as_u16(), again.code()), (404, "not_a_contact"));
    let sam_heard: i64 = b
        .count(
            "select count(*) from notification_outbox where user_id = $1",
            b.env.user("sam"),
        )
        .await?;
    assert_eq!(sam_heard, 0);
    Ok(())
}
