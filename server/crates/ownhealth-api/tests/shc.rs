//! `POST /api/shc` (slice 22): the person approves each issuer host before anything is fetched, a
//! JWKS on a private address is refused, and only a person who may import gets that far. The
//! signature, time and key checks are tested in `ownhealth-ingest-structured/tests/shc.rs`.

mod lane_i;

use std::io::Write as _;

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app};
use serde_json::{Value, json};

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A card from `iss` with a signature of zeros: enough to name its issuer, never to verify.
fn unsigned_card(iss: &str) -> String {
    let payload = json!({"iss": iss, "nbf": 1_790_000_000, "vc": {
        "type": ["https://smarthealth.cards#health-card"],
        "credentialSubject": {"fhirVersion": "4.0.1", "fhirBundle": {"resourceType": "Bundle", "type": "collection",
            "entry": [{"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
                "name": [{"family": "Rivera", "given": ["Alex"]}], "birthDate": "1979-05-14"}}]}}}});
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(&serde_json::to_vec(&payload).expect("payload"))
        .expect("deflate");
    let header = json!({"alg": "ES256", "zip": "DEF", "kid": "labnorte-test-kid"});
    format!(
        "{}.{}.{}",
        b64(&serde_json::to_vec(&header).expect("header")),
        b64(&enc.finish().expect("deflate")),
        b64(&[0u8; 64])
    )
}

fn shc_text(jws: &str) -> String {
    format!(
        "shc:/{}",
        jws.bytes().map(|b| format!("{:02}", b - b'-')).collect::<String>()
    )
}

async fn send(
    a: &App,
    who: &str,
    fields: &[(&str, &str)],
    file: Option<(&str, &[u8])>,
) -> anyhow::Result<(StatusCode, Value)> {
    let boundary = "ownhealth-shc-boundary";
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(
            format!("--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n").as_bytes(),
        );
    }
    if let Some((filename, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
                 Content-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
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

async fn documents(a: &App) -> anyhow::Result<i64> {
    a.count("select count(*) from document_source where channel = 'shc'")
        .await
}

#[tokio::test]
async fn a_card_waits_for_the_person_to_approve_its_issuer_host() -> R {
    let a = app().await?;
    let text = shc_text(&unsigned_card("https://issuer.labnorte.example"));
    let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &text)], None).await?;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v,
        json!({"status": "needs_approval",
               "issuers": [{"iss": "https://issuer.labnorte.example", "host": "issuer.labnorte.example"}]})
    );
    // approving another host changes nothing
    let (_, v) = send(
        &a,
        "alex",
        &[("profile", "alex"), ("text", &text), ("approve", "labnorte.example")],
        None,
    )
    .await?;
    assert_eq!(v["status"], "needs_approval");
    // the same card as a file
    let file = json!({"verifiableCredential": [unsigned_card("https://issuer.labnorte.example")]}).to_string();
    let (_, v) = send(
        &a,
        "alex",
        &[("profile", "alex")],
        Some(("alex.smart-health-card", file.as_bytes())),
    )
    .await?;
    assert_eq!(v["status"], "needs_approval");
    assert_eq!(documents(&a).await?, 0);
    Ok(())
}

#[tokio::test]
async fn an_approved_issuer_on_a_private_address_is_refused_without_a_connection() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let text = shc_text(&unsigned_card(&format!("https://127.0.0.1:{port}")));
    let host = format!("127.0.0.1:{port}");
    let (s, v) = send(
        &a,
        "alex",
        &[("profile", "alex"), ("text", &text), ("approve", &host)],
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["code"], "shc_refused");
    assert_eq!(v["detail"]["reason"], "private_issuer");
    assert_eq!(v["detail"]["iss"], format!("https://127.0.0.1:{port}"));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert_eq!(
        hits.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the private address was contacted"
    );
    assert_eq!(documents(&a).await?, 0);
    Ok(())
}

#[tokio::test]
async fn only_a_person_who_may_import_gets_an_answer_and_bad_forms_are_refused() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let text = shc_text(&unsigned_card(&format!("https://127.0.0.1:{port}")));
    // sam views alex's record but may not import into it: nothing is read or fetched
    let host = format!("127.0.0.1:{port}");
    let (s, v) = send(
        &a,
        "sam",
        &[("profile", "alex"), ("text", &text), ("approve", &host)],
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 0);

    let (s, v) = send(
        &a,
        "alex",
        &[("profile", "alex"), ("text", "a lab report from LabNorte")],
        None,
    )
    .await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(
        (v["detail"]["reason"].clone(), v["detail"]["iss"].clone()),
        (json!("no_card"), Value::Null)
    );
    let (s, _) = send(&a, "alex", &[("text", &text)], None).await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = send(
        &a,
        "alex",
        &[("profile", "alex"), ("text", &text)],
        Some(("x.png", b"x")),
    )
    .await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(documents(&a).await?, 0);
    Ok(())
}
