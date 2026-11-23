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
    assert!(!other.contains("break-glass"), "{other}");
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
    assert!(
        description.contains(&format!("https://testserver/family/break-glass/{id}")),
        "{description}"
    );
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
    let before = b.env.anon().get(&path).await?;
    assert_eq!(before.status, 200);
    let id = b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let after = b.env.anon().get(&path).await?;
    assert_eq!(after.status, 200, "{:?}", after.json);
    assert_eq!(b.uses(id).await?, 0);
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
    let item: Uuid = sqlx::query_scalar(
        "insert into review_item (household_id, profile_id, kind, payload) \
         values ($1, $2, 'agent_proposal', $3) returning id",
    )
    .bind(b.env.fam().household_id)
    .bind(b.alex_id())
    .bind(json!({"observation": {"category": "laboratory", "code_text": "LDL"}}))
    .fetch_one(b.env.pool())
    .await?;
    let mut sam = b.sam().await?;
    b.open_for(b.env.user("sam"), &mut sam, json!(["read:labs"])).await?;
    let r = sam.get(&format!("/api/review/{item}")).await?;
    assert_eq!(r.status, 200, "{:?}", r.json);
    for action in ["reject", "accept", "dismiss"] {
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
