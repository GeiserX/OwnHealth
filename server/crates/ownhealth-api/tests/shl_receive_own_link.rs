//! A SMART Health Link our own sender makes (routes/shl.rs, slice 50) is read by our own receiver
//! (slice 23) as it is: the receiver POSTs the manifest with its passcode to the instance's public
//! routes, served here on loopback, fetches the one file once, and imports the summary on channel
//! `shl`. The sender's access log names the receiver as the recipient.

#[path = "auth_support.rs"]
mod support;

use std::net::SocketAddr;

use axum::http::StatusCode;
use ownhealth_core::access::get_profile;
use ownhealth_core::net::Egress;
use ownhealth_ingest_structured::shl_receive::{self as shl, CardsChecked, Refusal};
use serde_json::json;
use support::Env;

type R = anyhow::Result<()>;

const SHARE: &str = "https://share.rivera.test";
/// Obviously fake test passcodes.
const PIN: &str = "test-pin-1234";
const WRONG: &str = "test-pin-0000";

#[tokio::test]
async fn a_link_of_our_own_sender_imports_through_our_own_receiver() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let r = alex
        .post(
            "/api/profiles/alex/shl",
            &json!({"label": "Para la consulta", "days": 3, "passcode": PIN}),
        )
        .await?;
    assert_eq!(r.status, StatusCode::CREATED, "{:?}", r.json);
    let text = r.json["shlink"].as_str().unwrap_or_default().to_string();

    // the instance's public routes on loopback, as the share host
    let app = env.anon().app;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await });
    let route = move |url: &str| match url.strip_prefix(SHARE) {
        Some(rest) if rest.starts_with('/') => (
            format!("http://127.0.0.1:{port}{rest}"),
            Egress {
                allow_private: true,
                https_only: false,
                ..Egress::connector()
            },
        ),
        // nothing else is served here: a request anywhere else fails the test, never leaves the box
        _ => panic!("the receiver reached for {url}, which this test does not serve"),
    };

    let link = shl::parse(&text, chrono::Utc::now())?;
    assert_eq!(link.host, "share.rivera.test");
    assert!(link.needs_passcode());
    assert_eq!(link.label.as_deref(), Some("Para la consulta"));
    let profile = {
        let mut c = env.pool().acquire().await?;
        get_profile(&mut c, env.profile("alex")).await?.expect("alex")
    };
    // our sender's 401 says how many attempts are left
    let wrong = shl::fetch(&link, Some(WRONG), &env.rt.blobs, profile.id, &route).await?;
    assert_eq!(wrong, Err(Refusal::Passcode(Some(4))));
    let held = shl::fetch(&link, Some(PIN), &env.rt.blobs, profile.id, &route).await??;
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].content_type, shl::FHIR);
    let opened = shl::open(&link, &held, &env.rt.blobs, profile.id).map_err(|s| anyhow::anyhow!("{s:?}"))?;
    let CardsChecked::Verified(cards) = shl::check(&opened, &[], &route, chrono::Utc::now()).await else {
        panic!("an IPS holds no cards");
    };
    let mut tx = env.pool().begin().await?;
    let got = shl::store(&mut tx, &env.rt.blobs, &profile, &link, &opened, cards, &[]).await?;
    tx.commit().await?;
    assert_eq!(got.documents.len(), 1);
    assert!(got.not_imported.is_empty());
    assert!(
        got.summary.accepted + got.summary.queued + got.summary.skipped > 0,
        "{:?}",
        got.summary
    );
    let channel: String = sqlx::query_scalar("select channel from document_source where document_id = $1")
        .bind(got.documents[0].document_id)
        .fetch_one(env.pool())
        .await?;
    assert_eq!(channel, "shl");
    // the sender logged the receiver by the name it sends, never the person's
    let recipients: Vec<(String, String)> = sqlx::query_as("select kind, recipient from shl_access order by id")
        .fetch_all(env.pool())
        .await?;
    assert_eq!(
        recipients,
        vec![
            ("passcode_wrong".to_string(), "OwnHealth".to_string()),
            ("manifest".to_string(), "OwnHealth".to_string()),
            ("file".to_string(), "OwnHealth".to_string()),
        ]
    );
    Ok(())
}
