//! Break-glass, server side (migration 1260): a contact the owner named asks for emergency access, the
//! owner's calendar feed carries the request at once, and access opens once the delay has passed and
//! that feed has served the request. Each test runs on its own throwaway database as the synthetic
//! Rivera family: Alex owns `alex`, Sam holds a viewer grant on it, and Carmen Ortega (a login of the
//! household made here) holds no grant on it.

#[path = "auth_support.rs"]
mod support;

#[path = "break_glass/get_routes.rs"]
mod get_routes;

use chrono::{DateTime, Duration, Utc};
use icalendar::{Calendar, CalendarComponent, CalendarDateTime, Component, DatePerhapsTime};
use ownhealth_core::access::{create_user, set_grant};
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::break_glass::settle;
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::observations::{CommitOptions, ObservationIn, commit_observation};
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::review::{ReviewItemIn, create_item};
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

    /// Alex names `user` with `categories`, they ask, Alex's feed carries the request, and it opens.
    async fn open_for(&mut self, user: Uuid, who: &mut Client, categories: Value) -> anyhow::Result<Uuid> {
        let r = self.name(user, json!({ "categories": categories })).await?;
        assert_eq!(r.status, 200, "{:?}", r.json);
        let id = self.ask(who).await?;
        self.fetch_feed().await?;
        let (_, opens, ..) = self.request(id).await?;
        self.settle_at(opens).await?;
        assert_eq!(self.request(id).await?.0, "active");
        Ok(id)
    }

    /// Sam's own grant on Alex's profile becomes `role` with `categories`.
    async fn grant_sam(&self, role: &str, categories: &[&str]) -> R {
        sqlx::query("update profile_grant set role = $3, categories = $4 where user_id = $1 and profile_id = $2")
            .bind(self.env.user("sam"))
            .bind(self.alex_id())
            .bind(role)
            .bind(categories)
            .execute(self.env.pool())
            .await?;
        Ok(())
    }

    /// The `break_glass.use` audit rows of one request.
    async fn uses(&self, id: Uuid) -> anyhow::Result<i64> {
        self.count(
            "select count(*) from audit_event where action = 'break_glass.use' and resource_ids = array[$1::text]",
            id,
        )
        .await
    }

    async fn text(&self, sql: &str, id: Uuid) -> anyhow::Result<String> {
        Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .bind(id)
            .fetch_one(self.env.pool())
            .await?)
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
        view.json["open_requests"][0]["waiting_for_calendar"], true,
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
    assert_eq!(view.json["open_requests"][0]["waiting_for_calendar"], false);

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
    for (step, says) in [
        ("cancel", "only a request still waiting can be cancelled"),
        ("revoke", "only an active request can be revoked"),
    ] {
        let r = b
            .alex
            .post0(&format!("/api/profiles/alex/break-glass/requests/{id}/{step}"))
            .await?;
        assert_eq!(r.status, 409, "{step} {:?}", r.json);
        assert!(r.json.to_string().contains(says), "{step} {:?}", r.json);
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
    // Sam reads Alex's records on his own grant and makes a feed of Alex's profile
    b.grant_sam("viewer", &["read:summary", "read:records"]).await?;
    let mut sam = b.sam().await?;
    let made = sam.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(made.status, 201, "{:?}", made.json);
    let id = b.ask(&mut carmen).await?;
    let (requested, opens): (DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as("select requested_at, activates_at from break_glass_request where id = $1")
            .bind(id)
            .fetch_one(b.env.pool())
            .await?;
    let pool = b.env.pool().clone();
    let fed = || {
        let pool = pool.clone();
        async move {
            let at: Option<DateTime<Utc>> =
                sqlx::query_scalar("select owner_feed_at from break_glass_request where id = $1")
                    .bind(id)
                    .fetch_one(&pool)
                    .await?;
            anyhow::Ok(at)
        }
    };
    // a feed of someone who does not own the profile, on the same profile, neither shows nor stamps
    // the request: the owner never heard of it
    let other = b.env.anon().get(made.json["path"].as_str().unwrap_or_default()).await?;
    assert_eq!(other.status, 200);
    let other = String::from_utf8(other.bytes)?;
    assert!(!other.contains("emergencia"), "{other}");
    assert_eq!(fed().await?, None);
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
    // the cancel page comes with the break-glass screens: until then the event links nowhere
    assert!(!description.contains("/family/break-glass/"), "{description}");
    // a generic feed names nobody
    assert!(!description.contains("Carmen"), "{description}");
    assert!(fed().await?.is_some());
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

/// Exports read with the caller's own grant: a summary reader keeps the IPS and the summary PDF while a
/// request is open, and neither carries what only the request opened.
#[tokio::test]
async fn exports_read_with_the_contacts_own_grant_only() -> R {
    let mut b = setup().await?;
    let mut sam = b.sam().await?;
    // the IPS sections and how many entries each holds
    let sections = |r: &Resp| -> Vec<(String, usize)> {
        r.json["entry"][0]["resource"]["section"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| {
                let code = s["code"]["coding"][0]["code"].as_str().unwrap_or_default().to_string();
                (code, s["entry"].as_array().map_or(0, Vec::len))
            })
            .collect()
    };
    let own = sam.post("/api/profiles/alex/export", &json!({"fmt": "ips"})).await?;
    assert_eq!(own.status, 200, "{:?}", own.json);
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs", "read:records"]))
        .await?;
    assert_eq!(me_alex(&mut sam).await?["break_glass"], true);
    let ips = sam.post("/api/profiles/alex/export", &json!({"fmt": "ips"})).await?;
    assert_eq!(ips.status, 200, "{:?}", ips.json);
    // the same Bundle as on his own grant: nothing the request opened
    assert!(!sections(&own).is_empty(), "{:?}", own.json);
    assert_eq!(sections(&ips), sections(&own));
    let pdf = sam.post("/api/profiles/alex/summary.pdf", &json!({})).await?;
    assert_eq!(pdf.status, 200, "{:?}", pdf.json);
    assert!(pdf.bytes.starts_with(b"%PDF"));
    // the next-draw PDF and a gap letter print labs and records his own grant lacks; the plan on
    // screen still reads through the request
    let r = sam.get("/api/profiles/alex/next-draw.pdf").await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let r = sam.get("/api/profiles/alex/next-draw").await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let r = sam.get("/api/profiles/alex/gaps/any/letter.pdf").await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);

    // Carmen reaches the profile only through her request: no summary PDF either
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(id).await?;
    b.settle_at(opens).await?;
    assert!(b.reads(&mut carmen).await?);
    let r = carmen.post("/api/profiles/alex/summary.pdf", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    Ok(())
}

/// A calendar feed reads as its creator with no browser session, so it never sees the request: a
/// contact with their own feed of the profile gets the same calendar, and no use is written.
#[tokio::test]
async fn a_contact_feed_never_carries_the_request() -> R {
    let mut b = setup().await?;
    b.grant_sam("viewer", &["read:summary", "read:records"]).await?;
    let mut sam = b.sam().await?;
    let made = sam.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(made.status, 201, "{:?}", made.json);
    let path = made.json["path"].as_str().unwrap_or_default().to_string();
    let before = calendar_events(b.env.anon().get(&path).await?)?;
    let id = b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let after = calendar_events(b.env.anon().get(&path).await?)?;
    assert_eq!(after, before);
    assert_eq!(b.uses(id).await?, 0);

    // Alex's unmeasured labs have due items. An ordinary grant that adds labs carries those events.
    let due = b.alex.get("/api/profiles/alex/due").await?;
    assert_eq!(due.status, 200, "{:?}", due.json);
    let lab_uids: Vec<String> = due.json["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| {
            i["labs"] == true && ["overdue", "due", "upcoming"].contains(&i["status"].as_str().unwrap_or_default())
        })
        .filter_map(|i| i["key"].as_str())
        .map(|key| ownhealth_api::routes::ics::uid(b.alex_id(), key))
        .collect();
    assert!(!lab_uids.is_empty(), "{:?}", due.json);
    let uids = |events: &[CalendarEvent]| events.iter().map(|e| e.0.clone()).collect::<Vec<_>>();
    assert!(lab_uids.iter().all(|u| !uids(&before).contains(u)), "{before:?}");
    b.grant_sam("viewer", &["read:summary", "read:records", "read:labs"])
        .await?;
    let with_labs = calendar_events(b.env.anon().get(&path).await?)?;
    assert!(lab_uids.iter().all(|u| uids(&with_labs).contains(u)), "{with_labs:?}");
    Ok(())
}

/// A feed made through break-glass would read without it and serve nothing: a contact whose own
/// grant lacks read:records cannot make one, even while the request adds it.
#[tokio::test]
async fn a_narrower_grant_makes_no_feed_through_break_glass() -> R {
    let mut b = setup().await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    assert!(b.reads(&mut sam).await?);
    let r = sam.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    Ok(())
}

/// A guardian contact keeps its own role, but it never shares what break-glass opened.
#[tokio::test]
async fn a_guardian_contact_cannot_share_what_break_glass_opened() -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let r = sam.post("/api/profiles/alex/shl", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "not_sharer"), "{:?}", r.json);
    let links: i64 = b
        .count("select count(*) from shl where profile_id = $1", b.alex_id())
        .await?;
    assert_eq!(links, 0);
    Ok(())
}

/// Break-glass reads only: a guardian contact cannot write through a category the request opened.
#[tokio::test]
async fn a_guardian_contact_cannot_write_through_what_break_glass_opened() -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    assert!(b.reads(&mut sam).await?);
    let r = sam.put("/api/profiles/alex/prevention-answers", &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    // on his own grant the same write goes through
    b.grant_sam("guardian", &["read:summary", "read:records"]).await?;
    let r = sam.put("/api/profiles/alex/prevention-answers", &json!({})).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    Ok(())
}

/// An emergency grant row (profile_grant.break_glass, from before requests) is refused a token and an
/// OAuth consent like an open request.
#[tokio::test]
async fn an_emergency_grant_row_gets_no_token_or_consent() -> R {
    let b = setup().await?;
    sqlx::query(
        "update profile_grant set break_glass = true, expires_at = now() + interval '14 days' \
         where user_id = $1 and profile_id = $2",
    )
    .bind(b.env.user("sam"))
    .bind(b.alex_id())
    .execute(b.env.pool())
    .await?;
    let mut sam = b.sam().await?;
    let r = sam
        .post(
            "/api/tokens",
            &json!({"label": "Claude", "profiles": ["alex"], "scopes": ["read:summary"]}),
        )
        .await?;
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
    let refused = ownhealth_auth::oauth::approve(&mut c, b.env.user("sam"), &consent).await;
    assert_eq!(
        refused
            .err()
            .and_then(|e| e.access_code().map(str::to_string))
            .as_deref(),
        Some("break_glass")
    );
    Ok(())
}

/// A refused request read nothing: it writes no use. An answered one writes one.
#[tokio::test]
async fn a_refused_request_writes_no_use() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(id).await?;
    b.settle_at(opens).await?;
    let r = carmen.get("/api/profiles/alex/break-glass").await?;
    assert_eq!(r.status, 403, "{:?}", r.json);
    let r = carmen
        .post(
            "/api/tokens",
            &json!({"label": "Claude", "profiles": ["alex"], "scopes": ["read:summary"]}),
        )
        .await?;
    assert_eq!(r.status, 403, "{:?}", r.json);
    assert_eq!(b.uses(id).await?, 0);
    assert!(b.reads(&mut carmen).await?);
    assert_eq!(b.uses(id).await?, 1);
    Ok(())
}

/// A request whose time ran out but that no sweep has ended yet reads nothing and cannot be revoked.
#[tokio::test]
async fn an_unswept_expired_request_neither_reads_nor_revokes() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    b.fetch_feed().await?;
    let (_, opens, ..) = b.request(id).await?;
    b.settle_at(opens).await?;
    sqlx::query("update break_glass_request set expires_at = now() - interval '1 second' where id = $1")
        .bind(id)
        .execute(b.env.pool())
        .await?;
    // straight to the core, with no REST request (whose settle would expire it first)
    let mut c = b.env.pool().acquire().await?;
    let open = ownhealth_core::break_glass::active(&mut c, b.carmen_id, Some(b.alex_id())).await?;
    assert!(open.is_empty(), "{open:?}");
    let revoked = ownhealth_core::break_glass::end(
        &mut c,
        b.alex_id(),
        id,
        b.env.user("alex"),
        ownhealth_core::break_glass::End::Revoke,
    )
    .await;
    assert!(matches!(revoked, Err(ownhealth_core::Error::Review(_))), "{revoked:?}");
    assert_eq!(b.request(id).await?.0, "active");
    Ok(())
}

/// A failing settle is logged, and every request goes on: an unopened request stays closed.
#[tokio::test]
async fn a_settle_that_fails_stops_no_request() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({"delay_hours": 12})).await?;
    let mut carmen = b.carmen().await?;
    let id = b.ask(&mut carmen).await?;
    sqlx::query(
        "update break_glass_request set requested_at = now() - interval '13 hours', \
         activates_at = now() - interval '1 hour' where id = $1",
    )
    .bind(id)
    .execute(b.env.pool())
    .await?;
    b.fetch_feed().await?;
    // from here on every settle fails: the due request cannot be moved
    sqlx::query(
        "create function bg_test_fail() returns trigger language plpgsql as \
         $$ begin raise exception 'settle fails here'; end $$",
    )
    .execute(b.env.pool())
    .await?;
    sqlx::query(
        "create trigger bg_test_fail before update on break_glass_request \
         for each row execute function bg_test_fail()",
    )
    .execute(b.env.pool())
    .await?;
    let me = b.alex.get("/api/me").await?;
    assert_eq!(me.status, 200, "{:?}", me.json);
    assert!(!b.reads(&mut carmen).await?);
    assert_eq!(b.request(id).await?.0, "requested");
    Ok(())
}

/// The reason is free text: a NUL byte is refused with a 422, not a 500; and the profile may be named by
/// its slug as well as its id.
#[tokio::test]
async fn a_request_takes_a_slug_and_refuses_a_nul_in_the_reason() -> R {
    let mut b = setup().await?;
    b.name(b.carmen_id, json!({})).await?;
    let mut carmen = b.carmen().await?;
    let r = carmen
        .post(
            "/api/break-glass/requests",
            &json!({"profile": "alex", "reason": "a\u{0}b"}),
        )
        .await?;
    assert_eq!(r.status, 422, "{:?}", r.json);
    assert_eq!(r.json["detail"][0]["loc"], json!(["body", "reason"]), "{:?}", r.json);
    let r = carmen
        .post(
            "/api/break-glass/requests",
            &json!({"profile": "alex", "reason": "En el hospital"}),
        )
        .await?;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(r.json["profile_id"], json!(b.alex_id()));
    // a profile that did not name her answers the same whether it exists or not
    for other in ["lucia", "nobody"] {
        let r = carmen
            .post(
                "/api/break-glass/requests",
                &json!({"profile": other, "reason": "En el hospital"}),
            )
            .await?;
        assert_eq!(
            (r.status.as_u16(), r.code()),
            (404, "not_a_contact"),
            "{other} {:?}",
            r.json
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_request_accepts_an_unhyphenated_profile_uuid() -> R {
    let mut b = setup().await?;
    let named = b.name(b.carmen_id, json!({})).await?;
    assert_eq!(named.status, 200, "{:?}", named.json);
    let mut carmen = b.carmen().await?;
    let r = carmen
        .post(
            "/api/break-glass/requests",
            &json!({"profile": b.alex_id().simple().to_string(), "reason": "En el hospital"}),
        )
        .await?;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(r.json["profile_id"], json!(b.alex_id()));
    Ok(())
}

#[tokio::test]
async fn a_request_prefers_a_profile_uuid_over_a_matching_slug() -> R {
    let mut b = setup().await?;
    let mut sam = b.sam().await?;
    let feed = sam.post("/api/profiles/sam/calendar-feeds", &json!({})).await?;
    assert_eq!(feed.status, 201, "{:?}", feed.json);
    let named = sam
        .put(
            &format!("/api/profiles/sam/break-glass/contacts/{}", b.carmen_id),
            &json!({}),
        )
        .await?;
    assert_eq!(named.status, 200, "{:?}", named.json);
    let alex_id = b.alex_id();
    sqlx::query("update profile set slug = $1 where id = $2")
        .bind(alex_id.to_string())
        .bind(b.env.profile("sam"))
        .execute(b.env.pool())
        .await?;

    let mut carmen = b.carmen().await?;
    let body = json!({"profile": alex_id, "reason": "En el hospital"});
    // A designation on the slug match does not grant emergency access to the UUID match.
    let r = carmen.post("/api/break-glass/requests", &body).await?;
    assert_eq!((r.status.as_u16(), r.code()), (404, "not_a_contact"), "{:?}", r.json);

    let named = b.name(b.carmen_id, json!({})).await?;
    assert_eq!(named.status, 200, "{:?}", named.json);
    let r = carmen.post("/api/break-glass/requests", &body).await?;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert_eq!(r.json["profile_id"], json!(alex_id));
    Ok(())
}

#[tokio::test]
async fn a_guardian_contact_cannot_retract_a_document_break_glass_opened() -> R {
    check_document_change_needs_own_scope("retract", json!({"reason": "x"})).await
}

#[tokio::test]
async fn a_guardian_contact_cannot_move_a_document_break_glass_opened() -> R {
    check_document_change_needs_own_scope("assign", json!({"profile": "sam"})).await
}

/// Check each change in its own test so a failure on one route cannot hide a failure on the other.
async fn check_document_change_needs_own_scope(action: &str, body: Value) -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let doc: Uuid = sqlx::query_scalar(
        "insert into document (household_id, profile_id, blob_sha256, filename, mime, size_bytes, doc_type) \
         values ($1, $2, $3, 'nota.pdf', 'application/pdf', 10, 'other') returning id",
    )
    .bind(b.env.fam().household_id)
    .bind(b.alex_id())
    .bind(vec![1u8; 32])
    .fetch_one(b.env.pool())
    .await?;
    let state = "select profile_id, status::text from document where id = $1";
    let before: (Option<Uuid>, String) = sqlx::query_as(state).bind(doc).fetch_one(b.env.pool()).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    let r = sam.get(&format!("/api/documents/{doc}")).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);

    let endpoint = format!("/api/documents/{doc}/{action}");
    let r = sam.post(&endpoint, &body).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let after: (Option<Uuid>, String) = sqlx::query_as(state).bind(doc).fetch_one(b.env.pool()).await?;
    assert_eq!(after, before);

    b.grant_sam("guardian", &["read:summary", "read:records"]).await?;
    let r = sam.post(&endpoint, &body).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    Ok(())
}

/// Break-glass reads only: a guardian contact sees a review item the request opened, and cannot
/// accept, reject, dismiss or reopen it.
#[tokio::test]
async fn a_guardian_contact_cannot_decide_a_review_item_break_glass_opened() -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let item = candidate(&b, None).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let r = sam.get(&format!("/api/review/{item}")).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    for action in ["accept", "reject", "dismiss"] {
        let r = sam
            .post(&format!("/api/review/{item}/{action}"), &json!({"reason": "x"}))
            .await?;
        assert_eq!(
            (r.status.as_u16(), r.code()),
            (403, "break_glass"),
            "{action}: {:?}",
            r.json
        );
    }
    let status = "select status from review_item where id = $1";
    assert_eq!(b.text(status, item).await?, "open");
    // Reject with his own grant so the item is eligible to reopen.
    b.grant_sam("guardian", &["read:summary", "read:labs"]).await?;
    let r = sam
        .post(&format!("/api/review/{item}/reject"), &json!({"reason": "x"}))
        .await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    b.grant_sam("guardian", &["read:summary"]).await?;
    let r = sam.post(&format!("/api/review/{item}/reopen"), &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    assert_eq!(b.text(status, item).await?, "rejected");
    Ok(())
}

type CalendarEvent = (String, Option<DatePerhapsTime>, String, String);

fn calendar_events(resp: Resp) -> anyhow::Result<Vec<CalendarEvent>> {
    assert_eq!(resp.status, 200, "{:?}", resp.json);
    let text = String::from_utf8(resp.bytes)?;
    let cal: Calendar = text.parse().map_err(anyhow::Error::msg)?;
    let mut events: Vec<_> = cal
        .components
        .iter()
        .filter_map(|component| {
            let CalendarComponent::Event(event) = component else {
                return None;
            };
            Some((
                event.get_uid().unwrap_or_default().to_string(),
                event.get_start(),
                event.get_summary().unwrap_or_default().to_string(),
                event.get_description().unwrap_or_default().to_string(),
            ))
        })
        .collect();
    events.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(events)
}

fn lab(profile: Uuid, value: &str, sensitive: bool) -> anyhow::Result<ObservationIn> {
    Ok(ObservationIn::from_json(json!({
        "profile_id": profile,
        "category": "laboratory",
        "printed_name": "Colesterol LDL",
        "printed_value": value,
        "printed_unit": "mg/dL",
        "value_num": value,
        "loinc": "13457-7",
        "alias_group": "ldl_c",
        "unit_ucum": "mg/dL",
        "value_canonical": value,
        "unit_canonical": "mg/dL",
        "derivation_version": "test",
        "effective_at": "2025-09-14T08:00:00+00:00",
        "sensitivity": if sensitive { json!(["sexual_health"]) } else { json!([]) },
    }))?)
}

async fn candidate(b: &Bg, document_id: Option<Uuid>) -> anyhow::Result<Uuid> {
    let profile = b.alex_id();
    let mut conn = b.env.pool().acquire().await?;
    Ok(create_item(
        &mut conn,
        &ReviewItemIn {
            household_id: Some(b.env.fam().household_id),
            profile_id: Some(profile),
            document_id,
            kind: "candidate_row".into(),
            payload: json!({
                "observation": lab(profile, "97", false)?,
                "src": {"kind": "manual", "profile_id": profile},
            }),
            ..Default::default()
        },
    )
    .await?)
}

async fn lab_document(b: &Bg) -> anyhow::Result<Uuid> {
    document(b, "lab_report").await
}

/// A one-page PDF of this type on Alex's profile.
async fn document(b: &Bg, doc_type: &'static str) -> anyhow::Result<Uuid> {
    let bytes = include_bytes!("../../ownhealth-core/tests/fixtures/demo-report.pdf");
    let profile = b.alex_id();
    let sha = b.env.rt.blobs.put_bytes(BlobOwner::Profile(profile), bytes)?;
    let mut conn = b.env.pool().acquire().await?;
    let doc = create_document(
        &mut conn,
        &NewDocument {
            doc_type: Some(doc_type),
            pages: Some(1),
            ..NewDocument::upload(
                b.env.fam().household_id,
                Some(profile),
                &sha,
                "application/pdf",
                bytes.len() as i64,
            )
        },
    )
    .await?;
    Ok(doc.id)
}

async fn identity_review_checks_source_scope(emergency: bool) -> R {
    ownhealth_ingest_pdf::register();
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let doc = lab_document(&b).await?;
    let target = b.env.profile("sam");
    let item = {
        let mut conn = b.env.pool().acquire().await?;
        create_item(
            &mut conn,
            &ReviewItemIn {
                household_id: Some(b.env.fam().household_id),
                profile_id: Some(b.alex_id()),
                document_id: Some(doc),
                kind: "document_identity".into(),
                payload: json!({"suggested_profile_id": target}),
                ..Default::default()
            },
        )
        .await?
    };
    let mut sam = b.sam().await?;
    if emergency {
        b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    }
    let endpoint = format!("/api/review/{item}/accept");
    let r = sam.post(&endpoint, &json!({})).await?;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (403, if emergency { "break_glass" } else { "insufficient_scope" }),
        "{:?}",
        r.json
    );
    assert_eq!(
        b.text("select status from review_item where id = $1", item).await?,
        "open"
    );
    assert_eq!(
        b.text("select profile_id::text from document where id = $1", doc)
            .await?,
        b.alex_id().to_string()
    );

    b.grant_sam("guardian", &["read:summary", "read:labs"]).await?;
    let r = sam.post(&endpoint, &json!({})).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(
        b.text("select profile_id::text from document where id = $1", doc)
            .await?,
        target.to_string()
    );
    Ok(())
}

#[tokio::test]
async fn identity_review_denies_a_move_with_only_emergency_document_scope() -> R {
    identity_review_checks_source_scope(true).await
}

#[tokio::test]
async fn identity_review_denies_a_move_without_document_scope_on_an_ordinary_grant() -> R {
    identity_review_checks_source_scope(false).await
}

#[tokio::test]
async fn bulk_review_needs_the_ordinary_document_scope() -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let doc = lab_document(&b).await?;
    let item = candidate(&b, Some(doc)).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let body = json!({"document_id": doc});
    let r = sam.post("/api/review/bulk-accept", &body).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    assert_eq!(
        b.text("select status from review_item where id = $1", item).await?,
        "open"
    );

    b.grant_sam("guardian", &["read:summary", "read:labs"]).await?;
    let r = sam.post("/api/review/bulk-accept", &body).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["accepted"], json!([item]));
    Ok(())
}

#[tokio::test]
async fn sensitive_labs_need_both_scopes_on_the_same_grant() -> R {
    let mut b = setup().await?;
    let profile = b.alex_id();
    let (plain, sensitive) = {
        let mut tx = b.env.pool().begin().await?;
        let src = SrcRef::New(SrcIn::manual(Some(b.env.user("alex"))));
        let plain = commit_observation(&mut tx, &lab(profile, "97", false)?, &src, CommitOptions::new("person"))
            .await?
            .id;
        let sensitive = commit_observation(&mut tx, &lab(profile, "111", true)?, &src, CommitOptions::new("person"))
            .await?
            .id;
        tx.commit().await?;
        (plain, sensitive)
    };
    b.grant_sam("viewer", &["read:sensitive"]).await?;
    let mut sam = b.sam().await?;
    let endpoint = "/api/profiles/alex/observations?category=laboratory";
    assert_eq!(sam.get(endpoint).await?.status, 403);
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let r = sam.get(endpoint).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let ids = |value: &Value| {
        value["items"]
            .as_array()
            .expect("observations contains an items array")
            .iter()
            .map(|item| item["id"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&r.json), vec![json!(plain)]);
    b.grant_sam("viewer", &["read:labs", "read:sensitive"]).await?;
    let r = sam.get(endpoint).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(ids(&r.json).len(), 2);
    assert!(ids(&r.json).contains(&json!(sensitive)));
    Ok(())
}

#[tokio::test]
async fn emergency_lab_access_cannot_quote_the_text_of_a_sensitive_report() -> R {
    let mut b = setup().await?;
    let profile = b.alex_id();
    let doc = lab_document(&b).await?;
    let text = "Colesterol LDL 97 mg/dL";
    let mut tx = b.env.pool().begin().await?;
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content)
         values ($1, 1, 595, 842, true, $2)",
    )
    .bind(doc)
    .bind(text)
    .execute(&mut *tx)
    .await?;
    let mut citation = SrcIn::new("document_region");
    citation.profile_id = Some(profile);
    citation.document_id = Some(doc);
    citation.page_no = Some(1);
    citation.bbox = Some([10.0, 20.0, 30.0, 40.0]);
    citation.actor_kind = "system".into();
    let plain = commit_observation(
        &mut tx,
        &lab(profile, "97", false)?,
        &SrcRef::New(citation.clone()),
        CommitOptions::new("import"),
    )
    .await?;
    citation.bbox = Some([10.0, 60.0, 30.0, 80.0]);
    commit_observation(
        &mut tx,
        &lab(profile, "111", true)?,
        &SrcRef::New(citation),
        CommitOptions::new("import"),
    )
    .await?;
    tx.commit().await?;

    b.grant_sam("viewer", &["read:sensitive", "read:documents_text"])
        .await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let endpoint = format!("/api/sources/{}?include_text=true", plain.src_id);
    let r = sam.get(&endpoint).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["text"], Value::Null);

    b.grant_sam("viewer", &["read:labs", "read:sensitive", "read:documents_text"])
        .await?;
    let r = sam.get(&endpoint).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["text"]["text"], text);
    Ok(())
}

#[tokio::test]
async fn only_an_emergency_read_fails_if_recording_a_use_fails() -> R {
    let mut b = setup().await?;
    b.grant_sam("viewer", &["read:records"]).await?;
    let mut sam = b.sam().await?;
    let id = b.open_for(b.env.user("sam"), &mut sam, json!(["read:summary"])).await?;
    sqlx::raw_sql(
        "create function reject_use() returns trigger language plpgsql as
         $$ begin raise exception 'cannot audit this use'; end $$;
         create trigger reject_use before insert on audit_event
         for each row when (new.action = 'break_glass.use') execute function reject_use();",
    )
    .execute(b.env.pool())
    .await?;
    let r = sam.get("/api/profiles/alex/status").await?;
    assert_eq!(r.status, 500, "{:?}", r.json);
    let r = sam.post("/api/profiles/alex/calendar-feeds", &json!({})).await?;
    assert_eq!(r.status, 201, "{:?}", r.json);
    assert!(r.json["path"].as_str().is_some());
    let r = sam.get("/api/profiles/alex/calendar-feeds").await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(
        b.count(
            "select count(*) from calendar_feed where created_by = $1",
            b.env.user("sam")
        )
        .await?,
        1
    );
    assert_eq!(b.uses(id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn the_owner_can_see_open_requests_older_than_the_history_window() -> R {
    let mut b = setup().await?;
    let mut sam = b.sam().await?;
    let active = b.open_for(b.env.user("sam"), &mut sam, json!(["read:summary"])).await?;
    let mut carmen = b.carmen().await?;
    assert_eq!(b.name(b.carmen_id, json!({})).await?.status, 200);
    let pending = b.ask(&mut carmen).await?;
    sqlx::query(
        "insert into break_glass_request
         (profile_id, contact_user_id, categories, reason, requested_at, activates_at, status, ended_at)
         select $1, $2, array['read:summary'], 'Request history',
                now() + i * interval '1 second', now() + interval '1 day', 'cancelled', now()
         from generate_series(1, 101) as i",
    )
    .bind(b.alex_id())
    .bind(b.carmen_id)
    .execute(b.env.pool())
    .await?;
    let r = b.alex.get("/api/profiles/alex/break-glass").await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let open = r.json["open_requests"]
        .as_array()
        .expect("open requests are separate from history");
    assert_eq!(open.len(), 2);
    assert!(open.iter().any(|r| r["id"] == json!(active) && r["status"] == "active"));
    assert!(
        open.iter()
            .any(|r| r["id"] == json!(pending) && r["status"] == "requested")
    );
    let history = r.json["requests"].as_array().expect("closed requests array");
    assert_eq!(history.len(), 100);
    assert!(history.iter().all(|r| r["status"] == "cancelled"));
    Ok(())
}

#[tokio::test]
async fn management_and_notice_reads_write_one_audit_each() -> R {
    let mut b = setup().await?;
    let mut carmen = b.carmen().await?;
    let owner = b.env.user("alex");
    for (client, endpoint, actor, tool, profile) in [
        (
            &mut b.alex,
            "/api/profiles/alex/break-glass",
            owner,
            "break_glass.owner_view",
            Some(b.env.profile("alex")),
        ),
        (&mut carmen, "/api/break-glass", b.carmen_id, "break_glass.mine", None),
    ] {
        assert_eq!(client.get(endpoint).await?.status, 200);
        assert_eq!(read_audits(&b.env, actor, tool, profile).await?, 1, "{endpoint}");
    }
    assert_eq!(b.alex.get("/api/notifications").await?.status, 200);
    assert_eq!(read_audits(&b.env, owner, "break_glass.notices", None).await?, 1);
    Ok(())
}

async fn read_audits(env: &Env, actor: Uuid, tool: &str, profile: Option<Uuid>) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "select count(*) from audit_event where action = 'read' and actor_id = $1 and tool = $2
         and profile_id is not distinct from $3",
    )
    .bind(actor)
    .bind(tool)
    .bind(profile)
    .fetch_one(env.pool())
    .await?)
}

#[tokio::test]
async fn me_counts_only_profiles_the_caller_can_review() -> R {
    let mut b = setup().await?;
    candidate(&b, None).await?;
    let mut carmen = b.carmen().await?;
    b.open_for(b.carmen_id, &mut carmen, json!(["read:summary"])).await?;
    let mut sam = b.sam().await?;
    for who in [&mut carmen, &mut sam] {
        let me = who.get("/api/me").await?;
        let review = who.get("/api/review").await?;
        assert_eq!(me.status, 200, "{:?}", me.json);
        assert_eq!(review.status, 200, "{:?}", review.json);
        assert_eq!(review.json["open"], 0);
        assert_eq!(me.json["review_open"], 0);
    }
    b.grant_sam("guardian", &["read:labs"]).await?;
    assert_eq!(sam.get("/api/me").await?.json["review_open"], 1);
    Ok(())
}

#[tokio::test]
async fn denial_suggests_ordinary_profiles_and_a_valid_emergency_slug_still_reads() -> R {
    let mut b = setup().await?;
    let mut carmen = b.carmen().await?;
    let id = b.open_for(b.carmen_id, &mut carmen, json!(["read:summary"])).await?;
    let unknown = carmen.get("/api/profiles/unknown/status").await?;
    assert_eq!((unknown.status.as_u16(), unknown.code()), (404, "profile_not_granted"));
    assert_eq!(unknown.json["detail"]["allowed"], json!(["carmen"]));
    assert_eq!(b.uses(id).await?, 0);
    assert!(b.reads(&mut carmen).await?);
    assert_eq!(b.uses(id).await?, 1);
    Ok(())
}

#[tokio::test]
async fn unread_request_and_activation_notices_outlast_the_recent_use_window() -> R {
    let mut b = setup().await?;
    let mut carmen = b.carmen().await?;
    let request = b.open_for(b.carmen_id, &mut carmen, json!(["read:summary"])).await?;
    sqlx::query(
        "insert into notification_outbox (user_id, profile_id, kind, request_id, created_at)
         select $1, $2, 'break_glass.use', $3, now() + i * interval '1 second'
         from generate_series(1, 201) as i",
    )
    .bind(b.env.user("alex"))
    .bind(b.alex_id())
    .bind(request)
    .execute(b.env.pool())
    .await?;
    let r = b.alex.get("/api/notifications").await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["unread"], 203);
    let notices = r.json["items"].as_array().expect("notification items array");
    let priority_ids: Vec<_> = notices
        .iter()
        .filter(|n| n["kind"] != "break_glass.use")
        .map(|n| n["id"].clone())
        .collect();
    assert_eq!(priority_ids.len(), 2);
    assert_eq!(notices.iter().filter(|n| n["kind"] == "break_glass.use").count(), 200);
    let r = b
        .alex
        .post("/api/notifications/read", &json!({"ids": priority_ids}))
        .await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["read"], 2);
    let r = b.alex.get("/api/notifications").await?;
    assert_eq!(r.json["items"].as_array().expect("notification items array").len(), 200);
    Ok(())
}
async fn record(b: &Bg, kind: &str, payload: Value) -> R {
    use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
    let mut conn = b.env.pool().acquire().await?;
    let row = RecordIn::from_json(kind, payload)?;
    let src = SrcRef::New(SrcIn::manual(Some(b.env.user("alex"))));
    add_record(&mut conn, &row, &src, RecordOptions::new("person")).await?;
    Ok(())
}

async fn seed_medications(b: &Bg) -> R {
    record(
        b,
        "medication",
        json!({
            "profile_id": b.alex_id(), "name": "Atorvastatina", "strength": "20 mg", "started": "2025-01-01",
        }),
    )
    .await?;
    record(
        b,
        "medication",
        json!({
            "profile_id": b.alex_id(), "name": "Truvada", "started": "2025-02-01", "sensitivity": ["sexual_health"],
        }),
    )
    .await
}

async fn seed_genome(b: &Bg) -> R {
    use ownhealth_genome::synthetic;
    let mut conn = b.env.pool().acquire().await?;
    synthetic::seed_genome(
        &mut conn,
        &b.env.rt.blobs,
        b.env.fam().household_id,
        b.alex_id(),
        &synthetic::alex_facts(),
        &[],
        "genome-facts.json",
        None,
    )
    .await?;
    Ok(())
}

async fn seed_conditions(b: &Bg) -> R {
    record(
        b,
        "condition",
        json!({
            "profile_id": b.alex_id(), "text": "Plain condition", "onset": "2021-11-09",
        }),
    )
    .await?;
    record(
        b,
        "condition",
        json!({
            "profile_id": b.alex_id(), "text": "Sensitive condition", "onset": "2023-01-01",
            "sensitivity": ["mental_health"],
        }),
    )
    .await
}

fn assert_visible(body: &Value, collection: &str, field: &str, plain: &str, sensitive: &str, show_sensitive: bool) {
    let rows = body[collection]
        .as_array()
        .expect("response has the requested collection");
    assert!(rows.iter().any(|r| r[field] == plain), "{body}");
    assert_eq!(rows.iter().any(|r| r[field] == sensitive), show_sensitive, "{body}");
}

async fn assert_scope_sources(
    b: &mut Bg,
    emergency: &str,
    ordinary: &[&str],
    paths: &[(&str, &str, &str, &str, &str)],
) -> R {
    let mut sam = b.sam().await?;
    b.grant_sam("viewer", ordinary).await?;
    b.open_for(b.env.user("sam"), &mut sam, json!([emergency])).await?;
    for &(path, collection, field, plain, sensitive) in paths {
        let r = sam.get(path).await?;
        assert_eq!(r.status, 200, "{path}: {:?}", r.json);
        assert_visible(&r.json, collection, field, plain, sensitive, false);
    }

    let mut full = ordinary.to_vec();
    full.push(emergency);
    b.grant_sam("viewer", &full).await?;
    for &(path, collection, field, plain, sensitive) in paths {
        let r = sam.get(path).await?;
        assert_eq!(r.status, 200, "{path}: {:?}", r.json);
        assert_visible(&r.json, collection, field, plain, sensitive, true);
    }
    Ok(())
}

#[tokio::test]
async fn records_list_keeps_sensitive_rows_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_conditions(&b).await?;
    assert_scope_sources(
        &mut b,
        "read:records",
        &["read:summary", "read:sensitive"],
        &[(
            "/api/profiles/alex/records/condition",
            "items",
            "title",
            "Plain condition",
            "Sensitive condition",
        )],
    )
    .await
}

#[tokio::test]
async fn timeline_keeps_sensitive_records_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_conditions(&b).await?;
    assert_scope_sources(
        &mut b,
        "read:records",
        &["read:summary", "read:sensitive"],
        &[
            (
                "/api/profiles/alex/timeline",
                "events",
                "title",
                "Plain condition",
                "Sensitive condition",
            ),
            (
                "/api/profiles/alex/timeline",
                "spans",
                "label",
                "Plain condition",
                "Sensitive condition",
            ),
        ],
    )
    .await
}

#[tokio::test]
async fn trend_keeps_sensitive_medicine_annotations_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_medications(&b).await?;
    assert_scope_sources(
        &mut b,
        "read:records",
        &["read:labs", "read:sensitive"],
        &[(
            "/api/profiles/alex/trend?code=ldl_c",
            "annotations",
            "label",
            "Atorvastatina 20 mg",
            "Truvada",
        )],
    )
    .await
}

#[tokio::test]
async fn family_history_keeps_sensitive_relatives_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    for (name, sensitivity) in [
        ("Plain relative", json!([])),
        ("Sensitive relative", json!(["mental_health"])),
    ] {
        let r = b
            .alex
            .post(
                "/api/profiles/alex/family",
                &json!({
                    "relationship": "FTH", "display_name": name, "sensitivity": sensitivity,
                }),
            )
            .await?;
        assert_eq!(r.status, 201, "{:?}", r.json);
    }
    assert_scope_sources(
        &mut b,
        "read:records",
        &["read:summary", "read:sensitive"],
        &[(
            "/api/profiles/alex/family",
            "items",
            "display_name",
            "Plain relative",
            "Sensitive relative",
        )],
    )
    .await
}

#[tokio::test]
async fn genome_keeps_sensitive_findings_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_genome(&b).await?;
    let r = b
        .alex
        .post("/api/profiles/alex/genome/consent", &json!({"ui": true}))
        .await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    let mut sam = b.sam().await?;
    b.grant_sam("viewer", &["read:summary", "read:sensitive"]).await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:genome"])).await?;
    let path = "/api/profiles/alex/genome";
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_visible(&r.json, "findings", "gene", "CYP2C19", "APOE", false);
    assert_eq!(r.json["locked"], json!([]));

    b.grant_sam("viewer", &["read:summary", "read:genome", "read:sensitive"])
        .await?;
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_visible(&r.json, "findings", "gene", "CYP2C19", "APOE", true);
    Ok(())
}

async fn document_page(b: &Bg, doc: Uuid) -> R {
    sqlx::query(
        "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok, text_content, fts)
         values ($1, 1, 595, 842, true, $2, to_tsvector('simple', $2))",
    )
    .bind(doc)
    .bind("LabNorte sampleword 97 mg/dL")
    .execute(b.env.pool())
    .await?;
    Ok(())
}

async fn record_uses(b: &Bg, id: Uuid) -> anyhow::Result<(i64, i64)> {
    Ok((
        b.uses(id).await?,
        b.count(
            "select count(*) from notification_outbox where request_id = $1 and kind = 'break_glass.use'",
            id,
        )
        .await?,
    ))
}

async fn assert_use_and_fail_closed(b: &Bg, sam: &mut Client, id: Uuid, path: &str) -> R {
    assert_eq!(record_uses(b, id).await?, (1, 1));
    sqlx::raw_sql(
        "create function reject_round5_use() returns trigger language plpgsql as
         $$ begin raise exception 'cannot audit this use'; end $$;
         create trigger reject_round5_use before insert on audit_event
         for each row when (new.action = 'break_glass.use') execute function reject_round5_use();",
    )
    .execute(b.env.pool())
    .await?;
    let r = sam.get(path).await?;
    assert_eq!(r.status, 500, "{:?}", r.json);
    assert_eq!(record_uses(b, id).await?, (1, 1));
    Ok(())
}

#[tokio::test]
async fn pgx_medicines_record_emergency_records_access() -> R {
    let mut b = setup().await?;
    seed_genome(&b).await?;
    seed_medications(&b).await?;
    b.grant_sam("viewer", &["read:genome"]).await?;
    let mut sam = b.sam().await?;
    let path = "/api/profiles/alex/genome";
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert!(r.json["pgx_grid"][0]["meds"].is_null());
    let id = b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert!(
        r.json["pgx_grid"].to_string().contains("Atorvastatina"),
        "{:?}",
        r.json["pgx_grid"]
    );
    assert_use_and_fail_closed(&b, &mut sam, id, path).await
}

#[tokio::test]
async fn document_search_records_emergency_text_access() -> R {
    let mut b = setup().await?;
    let doc = lab_document(&b).await?;
    document_page(&b, doc).await?;
    b.grant_sam("viewer", &["read:labs"]).await?;
    let mut sam = b.sam().await?;
    let path = "/api/profiles/alex/documents?q=sampleword";
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["items"], json!([]));
    let id = b
        .open_for(b.env.user("sam"), &mut sam, json!(["read:documents_text"]))
        .await?;
    assert_eq!(sam.get("/api/profiles/alex/documents").await?.status, 200);
    assert_eq!(record_uses(&b, id).await?, (0, 0));
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["items"][0]["id"], json!(doc));
    assert!(
        r.json["items"][0]["snippet"]["text"]
            .as_str()
            .is_some_and(|s| s.contains("sampleword"))
    );
    assert_use_and_fail_closed(&b, &mut sam, id, path).await
}

async fn seed_labs(b: &Bg) -> R {
    let mut tx = b.env.pool().begin().await?;
    let src = SrcRef::New(SrcIn::manual(Some(b.env.user("alex"))));
    commit_observation(
        &mut tx,
        &lab(b.alex_id(), "97", false)?,
        &src,
        CommitOptions::new("person"),
    )
    .await?;
    let mut sensitive = lab(b.alex_id(), "111", true)?;
    sensitive.effective_at += Duration::days(1);
    commit_observation(&mut tx, &sensitive, &src, CommitOptions::new("person")).await?;
    tx.commit().await?;
    Ok(())
}

#[tokio::test]
async fn trend_keeps_sensitive_lab_points_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_labs(&b).await?;
    assert_scope_sources(
        &mut b,
        "read:labs",
        &["read:sensitive"],
        &[(
            "/api/profiles/alex/trend?code=ldl_c",
            "points",
            "printed",
            "97 mg/dL",
            "111 mg/dL",
        )],
    )
    .await
}

#[tokio::test]
async fn timeline_keeps_sensitive_lab_draws_on_their_own_scope_source() -> R {
    let mut b = setup().await?;
    seed_labs(&b).await?;
    assert_scope_sources(
        &mut b,
        "read:labs",
        &["read:sensitive"],
        &[(
            "/api/profiles/alex/timeline?types=lab",
            "events",
            "date",
            "2025-09-14",
            "2025-09-15",
        )],
    )
    .await
}

#[tokio::test]
async fn sensitive_document_content_needs_both_scopes_on_the_same_grant() -> R {
    let mut b = setup().await?;
    let doc = lab_document(&b).await?;
    document_page(&b, doc).await?;
    let mut tx = b.env.pool().begin().await?;
    let mut observation = lab(b.alex_id(), "111", true)?;
    observation.document_id = Some(doc);
    let src = SrcRef::New(SrcIn::manual(Some(b.env.user("alex"))));
    commit_observation(&mut tx, &observation, &src, CommitOptions::new("person")).await?;
    tx.commit().await?;
    b.grant_sam("viewer", &["read:sensitive", "read:documents_text"])
        .await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    assert_eq!(sam.get(&format!("/api/documents/{doc}")).await?.status, 200);
    let content = [
        format!("/api/documents/{doc}/file"),
        format!("/api/documents/{doc}/pages/1.png"),
        format!("/api/documents/{doc}/pages/1/words"),
    ];
    for path in &content {
        let r = sam.get(path).await?;
        assert_eq!(
            (r.status.as_u16(), r.code()),
            (403, "insufficient_scope"),
            "{path}: {:?}",
            r.json
        );
    }
    b.grant_sam("viewer", &["read:labs", "read:sensitive", "read:documents_text"])
        .await?;
    for path in &content {
        let r = sam.get(path).await?;
        assert_eq!(r.status, 200, "{path}: {:?}", r.json);
    }
    Ok(())
}

#[tokio::test]
async fn bulk_review_skips_items_outside_the_ordinary_category_scope() -> R {
    let mut b = setup().await?;
    let doc = lab_document(&b).await?;
    let lab_row = candidate(&b, Some(doc)).await?;
    let records_row = {
        let mut conn = b.env.pool().acquire().await?;
        create_item(
            &mut conn,
            &ReviewItemIn {
                household_id: Some(b.env.fam().household_id),
                profile_id: Some(b.alex_id()),
                document_id: Some(doc),
                kind: "candidate_row".into(),
                payload: json!({
                    "record": {"kind": "condition", "text": "Record from the report"},
                    "observation": lab(b.alex_id(), "98", false)?,
                    "src": {"kind": "manual", "profile_id": b.alex_id()},
                }),
                ..Default::default()
            },
        )
        .await?
    };
    b.grant_sam("guardian", &["read:labs"]).await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    let path = "/api/review/bulk-accept";
    let body = json!({"document_id": doc});
    let r = sam.post(path, &body).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["accepted"], json!([lab_row]));
    assert_eq!(
        b.text("select status from review_item where id = $1", records_row)
            .await?,
        "open"
    );

    b.grant_sam("guardian", &["read:labs", "read:records"]).await?;
    let r = sam.post(path, &body).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(r.json["accepted"], json!([records_row]));
    Ok(())
}

/// A single accept checks the source document as bulk accept does: a lab row read from a document of
/// another type needs that document's scope on the reviewer's own grant.
#[tokio::test]
async fn single_review_needs_the_ordinary_scope_of_the_source_document() -> R {
    let mut b = setup().await?;
    b.grant_sam("guardian", &["read:labs"]).await?;
    let doc = document(&b, "discharge").await?;
    let item = candidate(&b, Some(doc)).await?;
    let mut sam = b.sam().await?;
    let endpoint = format!("/api/review/{item}/accept");
    let r = sam.post(&endpoint, &json!({})).await?;
    assert_eq!(
        (r.status.as_u16(), r.code()),
        (403, "insufficient_scope"),
        "{:?}",
        r.json
    );
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:records"])).await?;
    let r = sam.post(&endpoint, &json!({})).await?;
    assert_eq!((r.status.as_u16(), r.code()), (403, "break_glass"), "{:?}", r.json);
    let status = "select status from review_item where id = $1";
    assert_eq!(b.text(status, item).await?, "open");

    b.grant_sam("guardian", &["read:labs", "read:records"]).await?;
    let r = sam.post(&endpoint, &json!({})).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_eq!(b.text(status, item).await?, "accepted");
    Ok(())
}

/// The review counts (`review_open` on `/api/me`, `open` in the list) hold only what the list shows: a
/// lab row is left out for a reviewer without read:labs, and counted once an open request adds it.
#[tokio::test]
async fn review_counts_hold_only_what_the_list_shows() -> R {
    let mut b = setup().await?;
    candidate(&b, None).await?;
    b.grant_sam("guardian", &["read:summary"]).await?;
    let mut sam = b.sam().await?;
    let review = sam.get("/api/review").await?;
    assert_eq!(review.status, 200, "{:?}", review.json);
    assert_eq!(review.json["items"], json!([]), "{:?}", review.json);
    assert_eq!(review.json["withheld"], 1, "{:?}", review.json);
    assert_eq!(review.json["open"], 0, "{:?}", review.json);
    assert_eq!(sam.get("/api/me").await?.json["review_open"], 0);

    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let review = sam.get("/api/review").await?;
    assert_eq!(
        review.json["items"].as_array().map(Vec::len),
        Some(1),
        "{:?}",
        review.json
    );
    assert_eq!(review.json["open"], 1, "{:?}", review.json);
    assert_eq!(sam.get("/api/me").await?.json["review_open"], 1);
    Ok(())
}

/// History rows keep the sensitivity check of current rows: a correction that drops a condition's
/// sensitivity does not hand its sensitive first version to a reader without read:sensitive.
#[tokio::test]
async fn record_history_keeps_sensitive_versions_behind_read_sensitive() -> R {
    let mut b = setup().await?;
    let made = b
        .alex
        .post(
            "/api/profiles/alex/records/condition",
            &json!({"text": "Private condition", "onset": "2023-01-01", "sensitivity": ["mental_health"]}),
        )
        .await?;
    assert_eq!(made.status, 201, "{:?}", made.json);
    let id = made.json["id"].as_str().unwrap_or_default().to_string();
    let fixed = b
        .alex
        .post(
            &format!("/api/profiles/alex/records/condition/{id}/correct"),
            &json!({"text": "Plain condition", "onset": "2023-01-01"}),
        )
        .await?;
    assert_eq!(fixed.status, 201, "{:?}", fixed.json);
    let path = "/api/profiles/alex/records/condition?history=true";
    let mut sam = b.sam().await?;
    b.grant_sam("viewer", &["read:summary", "read:records"]).await?;
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_visible(&r.json, "items", "title", "Plain condition", "Private condition", false);
    b.grant_sam("viewer", &["read:summary", "read:records", "read:sensitive"])
        .await?;
    let r = sam.get(path).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    assert_visible(&r.json, "items", "title", "Plain condition", "Private condition", true);
    Ok(())
}
