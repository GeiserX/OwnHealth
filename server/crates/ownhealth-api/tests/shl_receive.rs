//! `POST /api/shc` with a SMART Health Link (slice 23): the person approves the link's host before
//! anything is fetched, a passcode link is never asked without its passcode, a host on a private
//! address is refused without a connection, and only a person who may import gets that far. The
//! fetch, decrypt and import are tested in `ownhealth-ingest-structured/tests/shl_receive.rs`, and
//! a link of our own sender end to end in `shl_receive_own_link.rs`. The last tests import a link
//! through the route itself, from a fake link server the dev issuer map points at (debug builds only).

mod lane_i;

use axum::body::Body;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::{Request, StatusCode, header};
use lane_i::{App, R, app};
use ownhealth_ingest_structured::{shc, shl_receive};
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

/// More `approve` fields than a link can need: its host, and one issuer host for each card of each of
/// its files (10 files of 20 cards, each from another host), so 201 is still a form and 202 is not.
#[tokio::test]
async fn a_link_whose_card_files_name_every_allowed_issuer_can_still_approve_its_own_host() -> R {
    let a = app().await?;
    let (port, hits) = listener().await?;
    let link = shlink(&format!("https://localhost:{port}/m"), None);
    let most = shl_receive::MAX_FILES * shc::MAX_CARDS + 1;
    assert_eq!(most, 201);
    let issuers: Vec<String> = (0..=most).map(|n| format!("issuer{n}.labnorte.example")).collect();
    let mut fields = vec![("profile", "alex"), ("text", link.as_str())];
    fields.extend(issuers[..most].iter().map(|h| ("approve", h.as_str())));
    let (s, v) = send(&a, "alex", &fields).await?;
    assert_eq!(
        (s, v["status"].clone()),
        (StatusCode::OK, json!("needs_approval")),
        "{v}"
    );
    fields.push(("approve", issuers[most].as_str()));
    let (s, _) = send(&a, "alex", &fields).await?;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(settle(&hits).await, 0);
    Ok(())
}

/// A pasted card envelope (JSON) with a `shlink:/` string in it is still read as cards: the card
/// receiver refuses this one for its own reason, and the link receiver never sees it.
#[tokio::test]
async fn a_card_envelope_that_mentions_a_link_stays_a_card() -> R {
    let a = app().await?;
    let link = shlink("https://shl.labnorte.example/m", None);
    let envelope = json!({"verifiableCredential": ["not.a.card"], "note": link}).to_string();
    let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &envelope)]).await?;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(v["detail"]["code"], "shc_refused", "{v}");
    Ok(())
}

// --------------------------------------------- a link imported through the route ----

/// The tests that import through the route reach their fake link server through the dev issuer map,
/// which only a debug build reads (routes/shc.rs `issuer_route`); a release build has no such map.
#[cfg(debug_assertions)]
mod mapped {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::State;
    use axum::response::IntoResponse;
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use http::{StatusCode, Uri};
    use ownhealth_auth::testing::{EcKey, b64u, jwk, signing_key};
    use ownhealth_core::Runtime;
    use ownhealth_core::blobs::BlobOwner;
    use ownhealth_core::demo::create_rivera_family;
    use ownhealth_core::jose;
    use ownhealth_core::testing::TestDb;
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};
    use uuid::Uuid;

    use super::lane_i::{App, R};
    use super::{documents, send};

    /// The fake link server's origin; the dev issuer map points it at a loopback port.
    const FAKE: &str = "https://shl.labnorte.example";
    const FAKE_HOST: &str = "shl.labnorte.example";
    /// A card issuer the dev issuer map points at the same fake.
    const ISS: &str = "https://issuer.labnorte.example";
    const ISS_HOST: &str = "issuer.labnorte.example";
    const KEY: [u8; 32] = [7; 32];
    /// An obviously fake test passcode.
    const PIN: &str = "test-pin-1234";

    /// A migrated database with the Rivera family, dev auth on (the dev issuer map needs it) and
    /// `FAKE` and `ISS` mapped to `port`. Requests still carry a session cookie, so each acts as the
    /// person named.
    async fn app_mapped(port: u16) -> anyhow::Result<App> {
        let db = TestDb::new().await?;
        let dir = tempfile::tempdir()?;
        let mut settings = db.settings(dir.path())?;
        settings.dev_auth = true;
        settings.dev_shc_issuers = Some(format!("{FAKE}=http://127.0.0.1:{port},{ISS}=http://127.0.0.1:{port}"));
        let rt = Runtime::with_pool(settings, db.pool.clone())?;
        let mut c = db.pool.acquire().await?;
        let family = create_rivera_family(&mut c, &format!("Rivera {}", ownhealth_core::ids::token_hex(3))).await?;
        drop(c);
        Ok(App { db, dir, rt, family })
    }

    fn fhir_file(family: &str, given: &str, birth: &str) -> String {
        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
                "name": [{"family": family, "given": [given]}], "birthDate": birth}},
            {"fullUrl": "resource:1", "resource": {"resourceType": "Immunization", "status": "completed",
                "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "207"}]},
                "patient": {"reference": "resource:0"}, "occurrenceDateTime": "2026-03-02"}}
        ]});
        jose::encrypt(&KEY, bundle.to_string().as_bytes(), Some("application/fhir+json"), true).expect("jwe")
    }

    /// The sha256 (hex) a received file is stored under.
    fn sha_of(file: &str) -> String {
        Sha256::digest(file.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// A link to the fake, with the key its files are sealed under.
    fn fake_link(flag: Option<&str>) -> String {
        let mut p = json!({"url": format!("{FAKE}/m"), "key": URL_SAFE_NO_PAD.encode(KEY),
            "exp": chrono::Utc::now().timestamp() + 86_400, "label": "LabNorte"});
        if let Some(f) = flag {
            p["flag"] = json!(f);
        }
        format!("shlink:/{}", URL_SAFE_NO_PAD.encode(p.to_string()))
    }

    /// What the fake does on each request: count it, lower a grant to viewer when one is given (the
    /// person's access changes while the link is fetched) and count the grants it lowered, then
    /// answer the manifest with `files`. It serves `jwks` as `ISS`'s keys, not counted.
    #[derive(Clone, Default)]
    struct Manifest {
        files: Value,
        lower: Option<(sqlx::PgPool, Uuid, Uuid)>,
        hits: Arc<AtomicUsize>,
        lowered: Arc<AtomicU64>,
        jwks: Option<Value>,
    }

    async fn manifest(State(m): State<Manifest>, uri: Uri) -> axum::response::Response {
        if uri.path() == "/.well-known/jwks.json" {
            return match &m.jwks {
                Some(j) => axum::Json(j.clone()).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            };
        }
        m.hits.fetch_add(1, Ordering::SeqCst);
        if let Some((pool, user, profile)) = &m.lower {
            let done = sqlx::query("update profile_grant set role = 'viewer' where user_id = $1 and profile_id = $2")
                .bind(user)
                .bind(profile)
                .execute(pool)
                .await
                .expect("lower the grant");
            m.lowered.fetch_add(done.rows_affected(), Ordering::SeqCst);
        }
        axum::Json(json!({"files": m.files})).into_response()
    }

    /// A bound loopback port, for the fake that starts once the app exists.
    async fn fake_port() -> anyhow::Result<(u16, tokio::net::TcpListener)> {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        Ok((l.local_addr()?.port(), l))
    }

    fn serve(l: tokio::net::TcpListener, m: Manifest) {
        let app = Router::new().fallback(manifest).with_state(m);
        tokio::spawn(async move { axum::serve(l, app).await });
    }

    fn one_file(file: &str) -> Value {
        json!([{"contentType": "application/fhir+json", "embedded": file}])
    }

    /// The link's host is asked about first and nothing is fetched; the same link, once its host is
    /// approved, reaches the fake and imports.
    #[tokio::test]
    async fn a_link_waits_for_the_person_to_approve_its_host_and_nothing_is_fetched() -> R {
        let (port, l) = fake_port().await?;
        let a = app_mapped(port).await?;
        let m = Manifest {
            files: one_file(&fhir_file("Rivera", "Alex", "1979-05-14")),
            ..Manifest::default()
        };
        serve(l, m.clone());
        let link = fake_link(Some("P"));
        for text in [link.clone(), format!("https://viewer.example.org/#{link}")] {
            let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &text), ("passcode", PIN)]).await?;
            assert_eq!(s, StatusCode::OK, "{v}");
            assert_eq!(v["status"], "needs_approval");
            assert_eq!(v["hosts"], json!([{"host": FAKE_HOST, "issuers": []}]));
            assert_eq!(v["link"]["host"], FAKE_HOST);
            assert_eq!(v["link"]["label"], "LabNorte");
            assert_eq!(
                (v["link"]["passcode"].clone(), v["link"]["direct"].clone()),
                (json!(true), json!(false))
            );
            assert!(v["link"]["expires"].is_string(), "{v}");
            assert!(!v.to_string().contains(PIN), "the passcode came back");
        }
        // approving another host changes nothing
        let (_, v) = send(
            &a,
            "alex",
            &[
                ("profile", "alex"),
                ("text", &link),
                ("approve", "labnorte.example"),
                ("passcode", PIN),
            ],
        )
        .await?;
        assert_eq!(v["status"], "needs_approval");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert_eq!(m.hits.load(Ordering::SeqCst), 0, "the link was fetched before the yes");
        assert_eq!(documents(&a).await?, 0);
        // the yes for its host: the same link reaches the fake, once, and imports
        let (s, v) = send(
            &a,
            "alex",
            &[
                ("profile", "alex"),
                ("text", &link),
                ("approve", FAKE_HOST),
                ("passcode", PIN),
            ],
        )
        .await?;
        assert_eq!((s, v["status"].clone()), (StatusCode::OK, json!("imported")), "{v}");
        assert_eq!(m.hits.load(Ordering::SeqCst), 1);
        assert_eq!(documents(&a).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn an_approved_link_imports_through_the_route_and_is_audited() -> R {
        let (port, l) = fake_port().await?;
        let a = app_mapped(port).await?;
        let file = fhir_file("Rivera", "Alex", "1979-05-14");
        serve(
            l,
            Manifest {
                files: one_file(&file),
                ..Manifest::default()
            },
        );
        let link = fake_link(None);
        let (s, v) = send(
            &a,
            "alex",
            &[("profile", "alex"), ("text", &link), ("approve", FAKE_HOST)],
        )
        .await?;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(
            (v["status"].clone(), v["identity"].clone()),
            (json!("imported"), json!("matched")),
            "{v}"
        );
        assert_eq!(v["link"]["host"], FAKE_HOST);
        assert_eq!(v["documents"].as_array().map(Vec::len), Some(1), "{v}");
        let alex = a.family.profile("alex");
        // bytes first: the file as received is stored for alex
        assert!(a.rt.blobs.exists(BlobOwner::Profile(alex), &sha_of(&file)));
        let audited = a
            .count(&format!(
                "select count(*) from audit_event where tool = 'POST /api/shc' and action = 'import' \
                 and profile_id = '{alex}' and resource_ids = array['{}']",
                v["documents"][0]["document_id"].as_str().unwrap_or_default()
            ))
            .await?;
        assert_eq!(audited, 1, "the import was not audited");
        assert_eq!(documents(&a).await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn held_files_sent_back_still_need_the_links_host_approved_and_keep_their_type() -> R {
        let (port, l) = fake_port().await?;
        let a = app_mapped(port).await?;
        let file = fhir_file("Rivera", "Alex", "1979-05-14");
        serve(
            l,
            Manifest {
                files: one_file(&file),
                ..Manifest::default()
            },
        );
        let link = fake_link(None);
        let (s, v) = send(
            &a,
            "alex",
            &[("profile", "alex"), ("text", &link), ("approve", FAKE_HOST)],
        )
        .await?;
        assert_eq!((s, v["status"].clone()), (StatusCode::OK, json!("imported")), "{v}");
        // the received file is stored under alex; sending its sha back without the yes for the host
        let sha = sha_of(&file);
        let held = format!("{sha} application/fhir+json");
        let (s, v) = send(&a, "alex", &[("profile", "alex"), ("text", &link), ("held", &held)]).await?;
        assert_eq!(
            (s, v["status"].clone()),
            (StatusCode::OK, json!("needs_approval")),
            "{v}"
        );
        assert_eq!(v["hosts"], json!([{"host": FAKE_HOST, "issuers": []}]));
        // the same file sent back as a card file: the server keeps to the type the file has
        let relabelled = format!("{sha} application/smart-health-card");
        let (s, v) = send(
            &a,
            "alex",
            &[
                ("profile", "alex"),
                ("text", &link),
                ("approve", FAKE_HOST),
                ("held", &relabelled),
            ],
        )
        .await?;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
        assert_eq!(
            (v["detail"]["code"].clone(), v["detail"]["reason"].clone()),
            (json!("shl_refused"), json!("held")),
            "{v}"
        );
        Ok(())
    }

    /// A SMART Health Card from `ISS` for Alex: DEFLATE, then ES256 over the compact parts, with the
    /// key's RFC 7638 thumbprint as its kid. Returns the card and the issuer's JWKS.
    fn signed_card() -> (String, Value) {
        use p256::ecdsa::Signature;
        use p256::ecdsa::signature::Signer as _;
        use std::io::Write as _;
        let key: EcKey = signing_key(Some(&[9; 32]));
        let mut k = jwk(&key, "");
        let thumb = format!(
            r#"{{"crv":"P-256","kty":"EC","x":"{}","y":"{}"}}"#,
            k["x"].as_str().unwrap_or_default(),
            k["y"].as_str().unwrap_or_default()
        );
        let kid = b64u(&Sha256::digest(thumb.as_bytes()));
        k["kid"] = json!(kid);
        let bundle = json!({"resourceType": "Bundle", "type": "collection", "entry": [
            {"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
                "name": [{"family": "Rivera", "given": ["Alex"]}], "birthDate": "1979-05-14"}},
            {"fullUrl": "resource:1", "resource": {"resourceType": "Immunization", "status": "completed",
                "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "207"}]},
                "patient": {"reference": "resource:0"}, "occurrenceDateTime": "2026-03-02"}}
        ]});
        let payload = json!({"iss": ISS, "nbf": chrono::Utc::now().timestamp() - 86_400, "vc": {
            "type": ["https://smarthealth.cards#health-card"],
            "credentialSubject": {"fhirVersion": "4.0.1", "fhirBundle": bundle}}});
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
        enc.write_all(payload.to_string().as_bytes()).expect("deflate");
        let header = json!({"alg": "ES256", "zip": "DEF", "kid": kid});
        let input = format!(
            "{}.{}",
            b64u(header.to_string().as_bytes()),
            b64u(&enc.finish().expect("deflate"))
        );
        let sig: Signature = key.sign(input.as_bytes());
        (format!("{input}.{}", b64u(&sig.to_bytes())), json!({"keys": [k]}))
    }

    /// A manifest file with an empty contentType whose JWE cty says it is a card file: the first
    /// answer reads it as one and asks about the card's issuer. The held files it returns carry the
    /// type the server read, so the client's retry after the yes imports, without a second fetch.
    #[tokio::test]
    async fn a_card_file_typed_only_by_its_jwe_imports_after_the_issuers_yes() -> R {
        let (port, l) = fake_port().await?;
        let a = app_mapped(port).await?;
        let (card, jwks) = signed_card();
        let card_file = jose::encrypt(
            &KEY,
            json!({"verifiableCredential": [card]}).to_string().as_bytes(),
            Some("application/smart-health-card"),
            true,
        )?;
        // untyped, no cty, and content we do not recognise: it is skipped, never a reason to refuse
        let odd_file = jose::encrypt(&KEY, b"a note the portal attached", None, true)?;
        let m = Manifest {
            files: json!([
                {"contentType": "", "embedded": card_file},
                {"contentType": "application/fhir+json", "embedded": fhir_file("Rivera", "Alex", "1979-05-14")},
                {"contentType": "", "embedded": odd_file}
            ]),
            jwks: Some(jwks),
            ..Manifest::default()
        };
        serve(l, m.clone());
        let link = fake_link(None);
        let (s, v) = send(
            &a,
            "alex",
            &[("profile", "alex"), ("text", &link), ("approve", FAKE_HOST)],
        )
        .await?;
        assert_eq!(
            (s, v["status"].clone()),
            (StatusCode::OK, json!("needs_approval")),
            "{v}"
        );
        assert_eq!(v["hosts"][0]["host"], ISS_HOST, "{v}");
        assert_eq!(
            v["held"],
            json!([
                {"sha": sha_of(&card_file), "content_type": "application/smart-health-card"},
                {"sha": sha_of(m.files[1]["embedded"].as_str().unwrap_or_default()),
                    "content_type": "application/fhir+json"},
                {"sha": sha_of(&odd_file), "content_type": "application/octet-stream"}
            ]),
            "{v}"
        );
        // the client sends each held file back as the answer gave it, with the yes for the issuer
        let held: Vec<String> = v["held"]
            .as_array()
            .map(|h| {
                h.iter()
                    .map(|f| {
                        format!(
                            "{} {}",
                            f["sha"].as_str().unwrap_or_default(),
                            f["content_type"].as_str().unwrap_or_default()
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let mut fields = vec![
            ("profile", "alex"),
            ("text", link.as_str()),
            ("approve", FAKE_HOST),
            ("approve", ISS_HOST),
        ];
        fields.extend(held.iter().map(|h| ("held", h.as_str())));
        let (s, v) = send(&a, "alex", &fields).await?;
        assert_eq!((s, v["status"].clone()), (StatusCode::OK, json!("imported")), "{v}");
        assert_eq!(v["documents"].as_array().map(Vec::len), Some(2), "{v}");
        assert_eq!(
            v["not_imported"],
            json!([{"content_type": "application/octet-stream", "reason": "unknown_type"}]),
            "{v}"
        );
        assert_eq!(m.hits.load(Ordering::SeqCst), 1, "the link was fetched again");
        Ok(())
    }

    /// Sam may import for Lucía when the request starts; the fake lowers that one grant before it
    /// answers. The import is refused, and the received file is not written under Lucía.
    #[tokio::test]
    async fn a_grant_lowered_while_the_link_is_fetched_stores_nothing() -> R {
        let (port, l) = fake_port().await?;
        let a = app_mapped(port).await?;
        let (sam, lucia) = (a.family.user("sam"), a.family.profile("lucia"));
        let file = fhir_file("Rivera", "Lucía", "2017-06-21");
        let m = Manifest {
            files: one_file(&file),
            lower: Some((a.rt.pool.clone(), sam, lucia)),
            ..Manifest::default()
        };
        serve(l, m.clone());
        let link = fake_link(None);
        // before: Sam passes the import check (the host question comes back), and nothing is fetched
        let (s, v) = send(&a, "sam", &[("profile", "lucia"), ("text", &link)]).await?;
        assert_eq!(
            (s, v["status"].clone()),
            (StatusCode::OK, json!("needs_approval")),
            "{v}"
        );
        assert_eq!(m.hits.load(Ordering::SeqCst), 0);
        let (s, v) = send(
            &a,
            "sam",
            &[("profile", "lucia"), ("text", &link), ("approve", FAKE_HOST)],
        )
        .await?;
        assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
        assert_eq!(
            m.lowered.load(Ordering::SeqCst),
            1,
            "the fake did not lower exactly one grant"
        );
        assert!(
            !a.rt.blobs.exists(BlobOwner::Profile(lucia), &sha_of(&file)),
            "the received file was written under the profile after the grant was lowered"
        );
        assert_eq!(documents(&a).await?, 0);
        let audited = a
            .count("select count(*) from audit_event where tool = 'POST /api/shc'")
            .await?;
        assert_eq!(audited, 0);
        Ok(())
    }
}
