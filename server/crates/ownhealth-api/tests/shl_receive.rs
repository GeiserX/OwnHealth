//! `POST /api/shc` with a SMART Health Link (slice 23): the person approves the link's host before
//! anything is fetched, a passcode link is never asked without its passcode, a host on a private
//! address is refused without a connection, and only a person who may import gets that far. The
//! fetch, decrypt and import are tested in `ownhealth-ingest-structured/tests/shl_receive.rs`, and
//! a link of our own sender end to end in `shl_receive_own_link.rs`.

mod lane_i;

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app};
use serde_json::{Value, json};

/// A link as ownhealth-ops `shl::shlink` writes it.
fn shlink(url: &str, flag: Option<&str>) -> String {
    let mut p = json!({"url": url, "key": URL_SAFE_NO_PAD.encode([7u8; 32]),
        "exp": chrono::Utc::now().timestamp() + 86_400, "label": "LabNorte"});
    if let Some(f) = flag {
        p["flag"] = json!(f);
    }
    format!("shlink:/{}", URL_SAFE_NO_PAD.encode(p.to_string()))
}

async fn send(a: &App, who: &str, fields: &[(&str, &str)]) -> anyhow::Result<(StatusCode, Value)> {
    let boundary = "ownhealth-shl-boundary";
    let mut body = String::new();
    for (name, value) in fields {
        body.push_str(&format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        ));
    }
    body.push_str(&format!("--{boundary}--\r\n"));
    let req = Request::post("/shc")
        .header(header::COOKIE, a.cookie(who).await?)
        .header(
            header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))?;
    a.send(req).await
}

/// A loopback listener that counts the connections it gets.
async fn listener() -> anyhow::Result<(u16, std::sync::Arc<std::sync::atomic::AtomicUsize>)> {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = l.local_addr()?.port();
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while l.accept().await.is_ok() {
            h.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    Ok((port, hits))
}

async fn settle(hits: &std::sync::atomic::AtomicUsize) -> usize {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    hits.load(std::sync::atomic::Ordering::SeqCst)
}

async fn documents(a: &App) -> anyhow::Result<i64> {
    a.count("select count(*) from document_source where channel in ('shl', 'shc')")
        .await
}

#[tokio::test]
async fn a_link_waits_for_the_person_to_approve_its_host_and_nothing_is_fetched() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let host = format!("localhost:{port}");
    let link = shlink(&format!("https://{host}/m"), Some("P"));
    for text in [link.clone(), format!("https://viewer.example.org/#{link}")] {
        let (s, v) = send(
            &a,
            "alex",
            &[("profile", "alex"), ("text", &text), ("passcode", "test-pin-1234")],
        )
        .await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["status"], "needs_approval");
        assert_eq!(v["hosts"], json!([{"host": host, "issuers": []}]));
        assert_eq!(v["link"]["host"], host.as_str());
        assert_eq!(v["link"]["label"], "LabNorte");
        assert_eq!(
            (v["link"]["passcode"].clone(), v["link"]["direct"].clone()),
            (json!(true), json!(false))
        );
        assert!(v["link"]["expires"].is_string(), "{v}");
        assert!(!v.to_string().contains("test-pin-1234"), "the passcode came back");
    }
    // approving another host changes nothing
    let (_, v) = send(
        &a,
        "alex",
        &[("profile", "alex"), ("text", &link), ("approve", "localhost")],
    )
    .await?;
    assert_eq!(v["status"], "needs_approval");
    assert_eq!(settle(&hits).await, 0, "the link was fetched before the yes");
    assert_eq!(documents(&a).await?, 0);
    Ok(())
}

#[tokio::test]
async fn an_approved_passcode_link_without_its_passcode_is_refused_before_any_request() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let host = format!("localhost:{port}");
    let link = shlink(&format!("https://{host}/m"), Some("P"));
    let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &link), ("approve", &host)]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(
        (v["detail"]["code"].clone(), v["detail"]["reason"].clone()),
        (json!("shl_refused"), json!("passcode_needed"))
    );
    assert_eq!(settle(&hits).await, 0);
    Ok(())
}

#[tokio::test]
async fn an_approved_host_on_a_private_address_is_refused_without_a_connection() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let host = format!("localhost:{port}");
    let link = shlink(&format!("https://{host}/m"), Some("P"));
    let fields = [
        ("profile", "alex"),
        ("text", link.as_str()),
        ("approve", host.as_str()),
        ("passcode", "test-pin-1234"),
    ];
    let (s, v) = send(&a, "alex", &fields).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(
        (v["detail"]["reason"].clone(), v["detail"]["detail"].clone()),
        (json!("private_host"), json!("localhost"))
    );
    assert!(!v.to_string().contains("test-pin-1234"));
    // a literal private address is refused before the person is asked
    let literal = shlink(&format!("https://127.0.0.1:{port}/m"), None);
    let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &literal)]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["reason"], "private_host");
    assert_eq!(settle(&hits).await, 0, "the private address was contacted");
    assert_eq!(documents(&a).await?, 0);
    Ok(())
}

#[tokio::test]
async fn only_a_person_who_may_import_gets_an_answer_and_bad_links_are_refused() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let host = format!("localhost:{port}");
    let link = shlink(&format!("https://{host}/m"), None);
    // sam views alex's record but may not import into it
    let (s, v) = send(&a, "sam", &[("profile", "alex"), ("text", &link), ("approve", &host)]).await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", "shlink:/bm90IGpzb24")]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["reason"], "link");
    let (s, _) = send(
        &a,
        "alex",
        &[
            ("profile", "alex"),
            ("text", &link),
            ("approve", &host),
            ("held", "not-a-sha application/fhir+json"),
        ],
    )
    .await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(settle(&hits).await, 0);
    Ok(())
}
