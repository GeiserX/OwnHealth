//! SMART Health Cards receive (slice 22), against issuer keys and cards these tests generate and an
//! issuer site they serve on loopback. Spec: SMART Health Cards Framework 1.4.0,
//! https://spec.smarthealth.cards/ . Nothing here reaches the internet.

mod common;

use std::io::{Cursor, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::routing::get;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Utc};
use common::{Env, R, family};
use ownhealth_core::access::get_profile;
use ownhealth_core::net::Egress;
use ownhealth_ingest_structured::shc::{self, Ask, Checked, Form, Input, Keys, Outcome, Received, Refusal};
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};

const ISS: &str = "https://issuer.labnorte.example";
const HOST: &str = "issuer.labnorte.example";
const HEALTH_CARD: &str = "https://smarthealth.cards#health-card";

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// 2026-11-21 12:00 UTC.
fn now() -> DateTime<Utc> {
    DateTime::from_timestamp(1_795_262_400, 0).unwrap_or_default()
}

fn day(n: i64) -> i64 {
    now().timestamp() + n * 86_400
}

/// An issuer key we generate: its public JWK, with the kid computed here by RFC 7638 3.2.
struct TestKey {
    key: SigningKey,
    jwk: Value,
}

fn test_key(seed: u8) -> TestKey {
    let key = SigningKey::from_slice(&[seed; 32]).expect("scalar");
    let point = key.verifying_key().to_sec1_point(false);
    let (x, y) = (b64(&point.as_bytes()[1..33]), b64(&point.as_bytes()[33..65]));
    let kid = b64(&Sha256::digest(
        format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#).as_bytes(),
    ));
    let jwk = json!({"kty": "EC", "kid": kid, "use": "sig", "alg": "ES256", "crv": "P-256", "x": x, "y": y});
    TestKey { key, jwk }
}

impl TestKey {
    fn kid(&self) -> String {
        self.jwk["kid"].as_str().unwrap_or_default().to_string()
    }
}

/// A vaccination for Alex Rivera, as an issuer writes it for a QR card ("Health Cards are Compact":
/// short resource: references, no ids).
fn bundle() -> Value {
    bundle_for("Alex", "Rivera", "1979-05-14")
}

fn bundle_for(given: &str, family: &str, born: &str) -> Value {
    json!({"resourceType": "Bundle", "type": "collection", "entry": [
        {"fullUrl": "resource:0", "resource": {"resourceType": "Patient",
            "name": [{"family": family, "given": [given]}], "birthDate": born}},
        {"fullUrl": "resource:1", "resource": {"resourceType": "Immunization", "status": "completed",
            "vaccineCode": {"coding": [{"system": "http://hl7.org/fhir/sid/cvx", "code": "207"}]},
            "patient": {"reference": "resource:0"}, "occurrenceDateTime": "2026-03-02",
            "lotNumber": "LN-0042", "performer": [{"actor": {"display": "LabNorte"}}]}}
    ]})
}

fn payload(iss: &str, nbf: i64, exp: Option<i64>) -> Value {
    payload_of(iss, nbf, exp, bundle())
}

fn payload_of(iss: &str, nbf: i64, exp: Option<i64>, bundle: Value) -> Value {
    let mut p = json!({"iss": iss, "nbf": nbf, "vc": {
        "type": [HEALTH_CARD, "https://smarthealth.cards#immunization"],
        "credentialSubject": {"fhirVersion": "4.0.1", "fhirBundle": bundle}}});
    if let Some(exp) = exp {
        p["exp"] = json!(exp);
    }
    p
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(data).expect("deflate");
    enc.finish().expect("deflate")
}

/// A compact JWS: this header, the payload minified and raw-DEFLATEd when the header says so.
fn sign(key: &SigningKey, header: &Value, payload: &Value) -> String {
    let body = serde_json::to_vec(payload).expect("payload");
    let body = if header.get("zip").is_some() {
        deflate(&body)
    } else {
        body
    };
    let input = format!("{}.{}", b64(&serde_json::to_vec(header).expect("header")), b64(&body));
    let sig: Signature = key.sign(input.as_bytes());
    format!("{input}.{}", b64(&sig.to_bytes()))
}

fn card(key: &TestKey, payload: &Value) -> String {
    sign(
        &key.key,
        &json!({"alg": "ES256", "zip": "DEF", "kid": key.kid()}),
        payload,
    )
}

/// "Encoding QRs", written here from the spec text: two digits per character, ord(c) - 45.
fn digits(jws: &str) -> String {
    jws.bytes().map(|b| format!("{:02}", b - b'-')).collect()
}

/// A fake issuer site on loopback: the JWKS and, when given, the revocation list of `kid`. It counts
/// every request.
async fn site(jwks: Value, crl: Option<(String, Value)>) -> anyhow::Result<(u16, Arc<AtomicUsize>)> {
    let mut paths = vec![("/.well-known/jwks.json".to_string(), jwks.to_string())];
    if let Some((kid, list)) = crl {
        paths.push((format!("/.well-known/crl/{kid}.json"), list.to_string()));
    }
    serve(paths).await
}

/// A loopback site that answers each path with its body (200) and anything else with 404, counting
/// every request.
async fn serve(paths: Vec<(String, String)>) -> anyhow::Result<(u16, Arc<AtomicUsize>)> {
    let hits = Arc::new(AtomicUsize::new(0));
    let mut app = Router::new();
    for (path, body) in paths {
        let h = hits.clone();
        app = app.route(
            &path,
            get(move || {
                h.fetch_add(1, Ordering::SeqCst);
                let body = body.clone();
                async move { body }
            }),
        );
    }
    // a request for any other path is counted too
    let h = hits.clone();
    let app = app.fallback(move || {
        h.fetch_add(1, Ordering::SeqCst);
        async { axum::http::StatusCode::NOT_FOUND }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    Ok((port, hits))
}

/// The test issuer `ISS` served from the loopback site; every other URL goes out as in production.
fn local(port: u16) -> impl Fn(&str) -> (String, Egress) + Send + Sync {
    move |url: &str| match url.strip_prefix(ISS) {
        Some(rest) => (
            format!("http://127.0.0.1:{port}{rest}"),
            Egress {
                allow_private: true,
                https_only: false,
                ..Egress::connector()
            },
        ),
        None => shc::public(url),
    }
}

async fn verify_at(jws: &str, port: u16) -> Result<shc::Card, Refusal> {
    shc::verify(jws, &mut Keys::default(), &local(port), now()).await
}

#[tokio::test]
async fn a_card_verifies_and_names_who_signed_it() -> R {
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    let got = verify_at(&card(&k, &payload(ISS, day(-30), Some(day(30)))), port).await?;
    assert_eq!(got.iss, ISS);
    assert_eq!(got.kid, k.kid());
    assert_eq!(got.issued.timestamp(), day(-30));
    assert_eq!(got.expires.map(|t| t.timestamp()), Some(day(30)));
    assert_eq!(got.bundle, bundle());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    // RFC 7519 4.1.5 leeway: an nbf a minute ahead of our clock still verifies
    verify_at(&card(&k, &payload(ISS, now().timestamp() + 60, None)), port).await?;
    Ok(())
}

#[tokio::test]
async fn a_bad_signature_is_refused() -> R {
    let (k, other) = (test_key(7), test_key(8));
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    // signed by another key under the issuer's kid
    let forged = sign(
        &other.key,
        &json!({"alg": "ES256", "zip": "DEF", "kid": k.kid()}),
        &payload(ISS, day(-1), None),
    );
    assert_eq!(verify_at(&forged, port).await, Err(Refusal::BadSignature));
    // a real card whose content was changed after signing: another lot number, the old signature
    let good = card(&k, &payload(ISS, day(-1), None));
    let mut parts: Vec<String> = good.split('.').map(str::to_string).collect();
    let mut changed = payload(ISS, day(-1), None);
    changed["vc"]["credentialSubject"]["fhirBundle"]["entry"][1]["resource"]["lotNumber"] = json!("LN-9999");
    parts[1] = b64(&deflate(&serde_json::to_vec(&changed)?));
    assert_eq!(verify_at(&parts.join("."), port).await, Err(Refusal::BadSignature));
    // one byte flipped in the compressed content: it no longer inflates, so it reads as damaged
    let mut raw = URL_SAFE_NO_PAD.decode(good.split('.').nth(1).unwrap_or_default())?;
    raw[3] ^= 1;
    parts[1] = b64(&raw);
    assert_eq!(verify_at(&parts.join("."), port).await, Err(Refusal::NotJws));
    Ok(())
}

#[tokio::test]
async fn an_expired_card_is_refused() -> R {
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let expired = card(&k, &payload(ISS, day(-60), Some(day(-1))));
    assert_eq!(
        verify_at(&expired, port).await,
        Err(Refusal::Expired("2026-11-20".into()))
    );
    Ok(())
}

#[tokio::test]
async fn a_card_valid_only_from_a_later_date_is_refused() -> R {
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let later = card(&k, &payload(ISS, day(2), None));
    assert_eq!(verify_at(&later, port).await, Err(Refusal::NotYet("2026-11-23".into())));
    Ok(())
}

#[tokio::test]
async fn a_kid_the_issuer_does_not_publish_or_that_is_not_the_thumbprint_is_refused() -> R {
    let (k, other) = (test_key(7), test_key(8));
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let unknown = sign(
        &k.key,
        &json!({"alg": "ES256", "zip": "DEF", "kid": other.kid()}),
        &payload(ISS, day(-1), None),
    );
    assert_eq!(verify_at(&unknown, port).await, Err(Refusal::UnknownKid));
    // the issuer lists the key under a kid that is not its thumbprint
    let mut renamed = k.jwk.clone();
    renamed["kid"] = json!("labnorte-key-1");
    let (port, _) = site(json!({"keys": [renamed]}), None).await?;
    let wrong = sign(
        &k.key,
        &json!({"alg": "ES256", "zip": "DEF", "kid": "labnorte-key-1"}),
        &payload(ISS, day(-1), None),
    );
    assert_eq!(
        verify_at(&wrong, port).await,
        Err(Refusal::Key("kid is not the key's thumbprint"))
    );
    Ok(())
}

#[tokio::test]
async fn a_jwks_on_a_private_address_is_refused_before_any_connection() -> R {
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    for (iss, host) in [
        (format!("https://127.0.0.1:{port}"), "127.0.0.1"),
        (format!("https://localhost:{port}"), "localhost"),
    ] {
        let jws = card(&k, &payload(&iss, day(-1), None));
        assert_eq!(
            shc::verify(&jws, &mut Keys::default(), &shc::public, now()).await,
            Err(Refusal::PrivateIssuer(host.into()))
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the private address was contacted");
    Ok(())
}

#[tokio::test]
async fn a_header_other_than_es256_with_zip_def_and_a_kid_is_refused() -> R {
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    let p = payload(ISS, day(-1), None);
    let cases = [
        (
            json!({"alg": "ES384", "zip": "DEF", "kid": k.kid()}),
            "alg is not ES256",
        ),
        (json!({"alg": "none", "zip": "DEF", "kid": k.kid()}), "alg is not ES256"),
        (json!({"alg": "ES256", "kid": k.kid()}), "zip is not DEF"),
        (json!({"alg": "ES256", "zip": "GZIP", "kid": k.kid()}), "zip is not DEF"),
        (json!({"alg": "ES256", "zip": "DEF"}), "there is no kid"),
    ];
    for (header, why) in cases {
        let jws = sign(&k.key, &header, &p);
        assert_eq!(shc::issuer(&jws), Err(Refusal::Header(why)), "{header}");
        assert_eq!(
            verify_at(&jws, port).await.map(|_| ()),
            Err(Refusal::Header(why)),
            "{header}"
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a refused header still fetched keys");
    Ok(())
}

#[tokio::test]
async fn a_card_the_issuer_revoked_is_refused() -> R {
    let k = test_key(7);
    let mut jwk = k.jwk.clone();
    jwk["crlVersion"] = json!(1);
    let crl = json!({"kid": k.kid(), "method": "rid", "ctr": 1, "rids": ["AQPCj4wwk6Mt", "lHKzqFUMjhs.1795000000"]});
    let (port, hits) = site(json!({"keys": [jwk]}), Some((k.kid(), crl))).await?;
    let with_rid = |rid: &str, nbf: i64| {
        let mut p = payload(ISS, nbf, None);
        p["vc"]["rid"] = json!(rid);
        card(&k, &p)
    };
    assert_eq!(
        verify_at(&with_rid("AQPCj4wwk6Mt", day(-1)), port).await,
        Err(Refusal::Revoked)
    );
    // revoked up to a time: an older card is refused, a newer one verifies
    assert_eq!(
        verify_at(&with_rid("lHKzqFUMjhs", 1_794_999_999), port).await,
        Err(Refusal::Revoked)
    );
    verify_at(&with_rid("lHKzqFUMjhs", 1_795_000_000), port).await?;
    verify_at(&with_rid("Other", day(-1)), port).await?;
    assert_eq!(
        hits.load(Ordering::SeqCst),
        8,
        "each verify_at has its own Keys: one JWKS and one list per card"
    );
    Ok(())
}

/// A QR code of `text` as a PNG, drawn by the qrcode crate (an independent encoder).
fn qr_png(texts: &[String]) -> Vec<u8> {
    let codes: Vec<image::GrayImage> = texts
        .iter()
        .map(|t| {
            qrcode::QrCode::new(t.as_bytes())
                .expect("fits a QR")
                .render::<image::Luma<u8>>()
                .module_dimensions(4, 4)
                .build()
        })
        .collect();
    let (w, h) = (
        codes.iter().map(|c| c.width() + 40).sum::<u32>().max(100),
        codes.iter().map(|c| c.height()).max().unwrap_or(0) + 40,
    );
    let mut canvas = image::GrayImage::from_pixel(w, h, image::Luma([255]));
    let mut x = 20;
    for c in &codes {
        image::imageops::overlay(&mut canvas, c, i64::from(x), 20);
        x += c.width() + 40;
    }
    let mut out = Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(canvas)
        .write_to(&mut out, image::ImageFormat::Png)
        .expect("png");
    out.into_inner()
}

#[test]
fn a_numeric_qr_code_round_trips() {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let text = format!("shc:/{}", digits(&jws));
    let png = qr_png(std::slice::from_ref(&text));
    assert_eq!(
        shc::read(&png),
        Ok(Input {
            form: Form::Photo("image/png"),
            cards: vec![jws.clone()]
        })
    );
    // the same text pasted
    assert_eq!(
        shc::read(format!("  {text}\n").as_bytes()).map(|i| i.cards),
        Ok(vec![jws])
    );
    // a photo with no QR code, and a QR code that is not a card
    assert_eq!(shc::read(&qr_png(&[])), Err(Refusal::NoCard("photo")));
    assert_eq!(
        shc::read(&qr_png(&["https://labnorte.example".to_string()])),
        Err(Refusal::NoCard("photo"))
    );
}

#[test]
fn a_card_split_over_several_qr_codes_is_put_back_together_in_any_order() {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let d = digits(&jws);
    // chunks of whole characters (two digits each)
    let third = (d.len() / 6) * 2;
    let parts = [&d[..third], &d[third..2 * third], &d[2 * third..]];
    let chunk = |c: usize| format!("shc:/{c}/3/{}", parts[c - 1]);
    let pasted = format!("{}\n{}\n{}", chunk(2), chunk(3), chunk(1));
    assert_eq!(shc::read(pasted.as_bytes()).map(|i| i.cards), Ok(vec![jws.clone()]));
    assert_eq!(
        shc::read(format!("{} {}", chunk(3), chunk(1)).as_bytes()),
        Err(Refusal::ChunkMissing { missing: 2, total: 3 })
    );
    // two QR codes in one photo
    let half = (d.len() / 4) * 2;
    let png = qr_png(&[format!("shc:/2/2/{}", &d[half..]), format!("shc:/1/2/{}", &d[..half])]);
    assert_eq!(shc::read(&png).map(|i| i.cards), Ok(vec![jws]));
    assert_eq!(shc::read(b"shc:/1/2/5676 shc:/1/3/5676"), Err(Refusal::Numeric));
    assert_eq!(shc::read(b"shc:/0/2/5676"), Err(Refusal::Numeric));
}

#[test]
fn a_smart_health_card_file_gives_every_credential() {
    let k = test_key(7);
    let (a, b) = (
        card(&k, &payload(ISS, day(-1), None)),
        card(&k, &payload(ISS, day(-2), None)),
    );
    let file = json!({"verifiableCredential": [a, b]}).to_string();
    assert_eq!(
        shc::read(file.as_bytes()),
        Ok(Input {
            form: Form::File,
            cards: vec![a, b]
        })
    );
    assert_eq!(
        shc::read(br#"{"resourceType": "Bundle"}"#),
        Err(Refusal::NoCard("file"))
    );
    assert_eq!(shc::read(b"a lab report from LabNorte"), Err(Refusal::NoCard("text")));
}

// ------------------------------------------------------------ with a database ----

async fn documents(conn: &mut sqlx::PgConnection, profile: uuid::Uuid) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("select count(*) from document where profile_id = $1")
            .bind(profile)
            .fetch_one(conn)
            .await?,
    )
}

#[tokio::test]
async fn an_approved_card_imports_its_vaccination_on_channel_shc() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let profile = get_profile(&mut tx, fam.profile("alex")).await?.expect("alex");
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    let text = format!("shc:/{}", digits(&card(&k, &payload(ISS, day(-30), None))));
    let input = shc::read(text.as_bytes())?;
    let approved = [HOST.to_string()];
    let received = Received {
        bytes: text.as_bytes(),
        filename: None,
        input: &input,
        approved: &approved,
    };
    let Outcome::Imported(got) = shc::receive(&mut tx, &env.blobs, &profile, &received, &local(port), now()).await?
    else {
        panic!("not imported");
    };
    assert!(got.created);
    assert_eq!(got.cards.len(), 1);
    assert_eq!(got.cards[0].iss, ISS);
    assert_eq!((got.summary.accepted, got.summary.queued), (1, 0), "{:?}", got.summary);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (channel, mime, signer): (String, String, Option<String>) = sqlx::query_as(
        "select s.channel, d.mime, d.flags -> 'shc' -> 0 ->> 'iss' from document d \
         join document_source s on s.document_id = d.id where d.id = $1",
    )
    .bind(got.document_id)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!((channel.as_str(), mime.as_str()), ("shc", "text/plain"));
    assert_eq!(signer.as_deref(), Some(ISS));
    let (code, lot, src_kind, src_doc): (Option<String>, Option<String>, String, Option<uuid::Uuid>) = sqlx::query_as(
        "select i.code, i.lot, s.kind, s.document_id from immunization i join src s on s.id = i.src_id \
         where i.profile_id = $1",
    )
    .bind(profile.id)
    .fetch_one(&mut *tx)
    .await?;
    assert_eq!((code.as_deref(), lot.as_deref()), (Some("207"), Some("LN-0042")));
    assert_eq!((src_kind.as_str(), src_doc), ("fhir_import", Some(got.document_id)));

    // the same card again: the same document, nothing added twice
    let Outcome::Imported(again) = shc::receive(&mut tx, &env.blobs, &profile, &received, &local(port), now()).await?
    else {
        panic!("not imported");
    };
    assert_eq!((again.document_id, again.created), (got.document_id, false));
    assert_eq!((again.summary.accepted, again.summary.skipped), (0, 1));
    Ok(())
}

#[tokio::test]
async fn a_host_the_person_has_not_approved_is_never_fetched_and_nothing_is_stored() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let profile = get_profile(&mut tx, fam.profile("alex")).await?.expect("alex");
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    let file = json!({"verifiableCredential": [card(&k, &payload(ISS, day(-1), None))]}).to_string();
    let input = shc::read(file.as_bytes())?;
    for approved in [vec![], vec!["other.example".to_string()]] {
        let received = Received {
            bytes: file.as_bytes(),
            filename: Some("alex.smart-health-card"),
            input: &input,
            approved: &approved,
        };
        assert_eq!(
            shc::receive(&mut tx, &env.blobs, &profile, &received, &local(port), now()).await?,
            Outcome::NeedsApproval(vec![Ask {
                host: HOST.into(),
                issuers: vec![ISS.into()]
            }])
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0, "the issuer was contacted");
    assert_eq!(documents(&mut tx, profile.id).await?, 0);
    Ok(())
}

#[tokio::test]
async fn a_refused_card_stores_nothing() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let fam = family(&mut tx).await?;
    let profile = get_profile(&mut tx, fam.profile("alex")).await?.expect("alex");
    let (k, other) = (test_key(7), test_key(8));
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    // a good card and a forged one in one file: neither is imported
    let file = json!({"verifiableCredential": [
        card(&k, &payload(ISS, day(-1), None)),
        sign(&other.key, &json!({"alg": "ES256", "zip": "DEF", "kid": k.kid()}), &payload(ISS, day(-1), None)),
    ]})
    .to_string();
    let input = shc::read(file.as_bytes())?;
    let approved = [HOST.to_string()];
    let received = Received {
        bytes: file.as_bytes(),
        filename: None,
        input: &input,
        approved: &approved,
    };
    assert_eq!(
        shc::receive(&mut tx, &env.blobs, &profile, &received, &local(port), now()).await?,
        Outcome::Refused {
            refusal: Refusal::BadSignature,
            iss: Some(ISS.into())
        }
    );
    assert_eq!(documents(&mut tx, profile.id).await?, 0);
    let immunizations: i64 = sqlx::query_scalar("select count(*) from immunization where profile_id = $1")
        .bind(profile.id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(immunizations, 0);
    Ok(())
}

// ------------------------------------------------------------ round 1 ----

fn fixture(name: &str) -> Vec<u8> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../fixtures/shc/");
    std::fs::read(format!("{path}{name}")).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn fixture_text(name: &str) -> String {
    String::from_utf8(fixture(name)).unwrap_or_default().trim().to_string()
}

/// A printed card photographed on a desk: rotated, blurred, keystoned, unevenly lit, JPEG. The
/// spec's validator (jsQR) reads it.
#[test]
fn a_photo_of_a_printed_card_reads_its_card() {
    assert_eq!(
        shc::read(&fixture("vaccine-photo.jpg")),
        Ok(Input {
            form: Form::Photo("image/jpeg"),
            cards: vec![fixture_text("vaccine-photo.jws")]
        })
    );
}

/// A sheet with the 3 QR codes of a split card, photographed the same way.
#[test]
fn a_photo_of_a_three_code_sheet_reads_every_code() {
    assert_eq!(
        shc::read(&fixture("three-code-sheet.jpg")).map(|i| i.cards),
        Ok(vec![fixture_text("three-code-sheet.jws")])
    );
}

#[test]
fn a_photo_with_only_some_codes_of_a_split_card_says_how_many_were_read() {
    let k = test_key(7);
    let d = digits(&card(&k, &payload(ISS, day(-1), None)));
    let third = (d.len() / 6) * 2;
    let png = qr_png(&[
        format!("shc:/1/3/{}", &d[..third]),
        format!("shc:/3/3/{}", &d[2 * third..]),
    ]);
    assert_eq!(shc::read(&png), Err(Refusal::ChunksUnread { found: 2, total: 3 }));
}

/// A large, grainy photo: a 3800 px code with one pixel in ten flipped. The QR search does not find
/// it at full size and does once the photo is scaled down, which averages the grain away.
#[test]
fn a_large_grainy_photo_is_read_at_a_smaller_size() {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let code = qrcode::QrCode::new(format!("shc:/{}", digits(&jws)).as_bytes())
        .expect("fits a QR")
        .render::<image::Luma<u8>>()
        .module_dimensions(1, 1)
        .build();
    let side = 3800;
    let mut img = image::imageops::resize(&code, side, side, image::imageops::FilterType::Nearest);
    let mut seed: u64 = 3;
    for _ in 0..u64::from(side) * u64::from(side) / 10 {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let (x, y) = ((seed >> 40) as u32 % side, ((seed >> 16) & 0xff_ffff) as u32 % side);
        let p = img.get_pixel_mut(x, y);
        p.0[0] = 255 - p.0[0];
    }
    let mut png = Cursor::new(Vec::new());
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut png, image::ImageFormat::Png)
        .expect("png");
    assert_eq!(shc::read(png.get_ref()).map(|i| i.cards), Ok(vec![jws]));
}

/// A PNG header that claims 9000 x 9000 pixels, with no pixel data: refused from the header.
fn huge_png_header() -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&9000u32.to_be_bytes());
    ihdr.extend_from_slice(&9000u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(&b"IHDR"[..], ihdr.as_slice()), (&b"IEND"[..], &[][..])] {
        out.extend_from_slice(&u32::try_from(data.len()).unwrap_or(0).to_be_bytes());
        let mut crc = flate2::Crc::new();
        crc.update(kind);
        crc.update(data);
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc.sum().to_be_bytes());
    }
    out
}

#[test]
fn a_photo_larger_than_8192_pixels_is_refused_at_once() {
    let started = Instant::now();
    assert_eq!(shc::read(&huge_png_header()), Err(Refusal::PhotoTooLarge));
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
}

#[test]
fn chunks_pasted_back_to_back_still_read() {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let d = digits(&jws);
    let half = (d.len() / 4) * 2;
    let pasted = format!("shc:/2/2/{}shc:/1/2/{}", &d[half..], &d[..half]);
    assert_eq!(shc::read(pasted.as_bytes()).map(|i| i.cards), Ok(vec![jws]));
}

#[test]
fn a_chunk_numbered_past_its_count_or_given_twice_with_other_digits_is_refused() {
    let k = test_key(7);
    let d = digits(&card(&k, &payload(ISS, day(-1), None)));
    let half = (d.len() / 4) * 2;
    assert_eq!(
        shc::read(format!("shc:/3/2/{}", &d[..half]).as_bytes()),
        Err(Refusal::Numeric)
    );
    let twice = format!(
        "shc:/1/2/{} shc:/1/2/{} shc:/2/2/{}",
        &d[..half],
        &d[2..half + 2],
        &d[half..]
    );
    assert_eq!(shc::read(twice.as_bytes()), Err(Refusal::Numeric));
}

#[test]
fn more_than_twenty_cards_in_one_import_are_refused() {
    let k = test_key(7);
    let one = format!("shc:/{}", digits(&card(&k, &payload(ISS, day(-1), None))));
    let text = vec![one.as_str(); 21].join("\n");
    assert_eq!(shc::read(text.as_bytes()), Err(Refusal::TooMany));
    let jws = card(&k, &payload(ISS, day(-1), None));
    let file = json!({"verifiableCredential": vec![jws; 21]}).to_string();
    assert_eq!(shc::read(file.as_bytes()), Err(Refusal::TooMany));
}

#[tokio::test]
async fn nbf_allows_five_minutes_of_clock_skew_and_exp_allows_none() -> R {
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let t = now().timestamp();
    verify_at(&card(&k, &payload(ISS, t + 299, None)), port).await?;
    assert_eq!(
        verify_at(&card(&k, &payload(ISS, t + 301, None)), port).await,
        Err(Refusal::NotYet("2026-11-21".into()))
    );
    verify_at(&card(&k, &payload(ISS, day(-1), Some(t + 1))), port).await?;
    assert_eq!(
        verify_at(&card(&k, &payload(ISS, day(-1), Some(t - 1))), port).await,
        Err(Refusal::Expired("2026-11-21".into()))
    );
    Ok(())
}

#[tokio::test]
async fn a_key_that_is_private_or_not_marked_for_es256_signing_is_refused() -> R {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let mut with_d = k.jwk.clone();
    with_d["d"] = json!("c2VjcmV0");
    let mut no_use = k.jwk.clone();
    if let Some(o) = no_use.as_object_mut() {
        o.shift_remove("use");
    }
    let mut es384 = k.jwk.clone();
    es384["alg"] = json!("ES384");
    for (jwk, why) in [
        (with_d, "it carries a private part"),
        (no_use, "it is not marked use sig and alg ES256"),
        (es384, "it is not marked use sig and alg ES256"),
    ] {
        let (port, _) = site(json!({"keys": [jwk]}), None).await?;
        assert_eq!(verify_at(&jws, port).await, Err(Refusal::Key(why)), "{why}");
    }
    Ok(())
}

#[tokio::test]
async fn a_credential_without_the_health_card_type_is_refused() -> R {
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let mut p = payload(ISS, day(-1), None);
    p["vc"]["type"] = json!(["https://smarthealth.cards#immunization"]);
    assert_eq!(
        verify_at(&card(&k, &p), port).await,
        Err(Refusal::Payload("vc.type has no health-card type"))
    );
    Ok(())
}

#[tokio::test]
async fn a_card_that_inflates_past_one_mib_is_refused_before_any_fetch() -> R {
    let k = test_key(7);
    let (port, hits) = site(json!({"keys": [k.jwk]}), None).await?;
    let mut big = bundle();
    big["entry"][0]["resource"]["name"][0]["text"] = json!("A".repeat(1024 * 1024));
    let jws = card(&k, &payload_of(ISS, day(-1), None, big));
    assert_eq!(
        verify_at(&jws, port).await,
        Err(Refusal::Payload("it is larger than 1 MiB"))
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    Ok(())
}

fn with_rid(k: &TestKey, rid: &str) -> String {
    let mut p = payload(ISS, day(-1), None);
    p["vc"]["rid"] = json!(rid);
    card(k, &p)
}

#[tokio::test]
async fn a_revocation_list_is_fetched_only_for_a_key_with_crl_version() -> R {
    let k = test_key(7);
    let crl = json!({"kid": k.kid(), "method": "rid", "ctr": 1, "rids": ["AQPCj4wwk6Mt"]});
    let (port, hits) = site(json!({"keys": [k.jwk]}), Some((k.kid(), crl))).await?;
    verify_at(&with_rid(&k, "AQPCj4wwk6Mt"), port).await?;
    assert_eq!(hits.load(Ordering::SeqCst), 1, "a key without crlVersion has no list");
    Ok(())
}

#[tokio::test]
async fn a_card_whose_revocation_list_cannot_be_read_is_refused() -> R {
    let k = test_key(7);
    let mut jwk = k.jwk.clone();
    jwk["crlVersion"] = json!(1);
    let jws = with_rid(&k, "AQPCj4wwk6Mt");
    let list = format!("/.well-known/crl/{}.json", k.kid());
    let jwks = ("/.well-known/jwks.json".to_string(), json!({"keys": [jwk]}).to_string());
    for (crl, why) in [
        (None, "http_404"),
        (Some(json!({}).to_string()), "not_crl"),
        (Some(json!({"kid": k.kid(), "rids": null}).to_string()), "not_crl"),
        (Some("rids".to_string()), "not_json"),
        (Some(json!({"kid": "another-kid", "rids": []}).to_string()), "other_kid"),
    ] {
        let mut paths = vec![jwks.clone()];
        if let Some(body) = crl {
            paths.push((list.clone(), body));
        }
        let (port, _) = serve(paths).await?;
        assert_eq!(
            verify_at(&jws, port).await,
            Err(Refusal::RevocationList(why.into())),
            "{why}"
        );
    }
    Ok(())
}

/// "Revocation" (SHC 1.4.0): when the key has `crlVersion`, verifiers SHALL download the list, with
/// no condition on the card's rid. A card with no rid is refused when the list cannot be read, and
/// imported when it can.
#[tokio::test]
async fn a_card_without_a_rid_is_checked_against_the_list_too() -> R {
    let k = test_key(7);
    let mut jwk = k.jwk.clone();
    jwk["crlVersion"] = json!(1);
    let jws = card(&k, &payload(ISS, day(-1), None));
    let jwks = ("/.well-known/jwks.json".to_string(), json!({"keys": [jwk]}).to_string());
    let (port, _) = serve(vec![jwks]).await?;
    assert_eq!(
        verify_at(&jws, port).await.map(|_| ()),
        Err(Refusal::RevocationList("http_404".into())),
        "no list"
    );
    let crl = json!({"kid": k.kid(), "method": "rid", "ctr": 1, "rids": ["AQPCj4wwk6Mt"]});
    let (port, hits) = site(json!({"keys": [jwk]}), Some((k.kid(), crl))).await?;
    assert_eq!(verify_at(&jws, port).await.map(|_| ()), Ok(()), "a readable list");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the keys and the list");
    Ok(())
}

/// "Revocation": a key's `crlVersion` is the list's update counter. A list whose `ctr` is lower is
/// an old copy, and it cannot show that the card was not revoked since.
#[tokio::test]
async fn a_revocation_list_older_than_the_key_says_is_refused() -> R {
    let k = test_key(7);
    let mut jwk = k.jwk.clone();
    jwk["crlVersion"] = json!(2);
    let jws = with_rid(&k, "AQPCj4wwk6Mt");
    for (ctr, want) in [
        (json!(1), Err(Refusal::RevocationList("old_list".into()))),
        (json!(2), Ok(())),
        (json!("3"), Ok(())),
    ] {
        let crl = json!({"kid": k.kid(), "method": "rid", "ctr": ctr, "rids": []});
        let (port, _) = site(json!({"keys": [jwk]}), Some((k.kid(), crl))).await?;
        assert_eq!(verify_at(&jws, port).await.map(|_| ()), want, "ctr {ctr}");
    }
    Ok(())
}

/// A bundle entry with no resource type cannot be imported, so the card is refused before anything
/// is stored.
#[tokio::test]
async fn a_bundle_entry_without_a_resource_type_is_refused() -> R {
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let mut b = bundle();
    b["entry"][1]["resource"] = json!({"status": "completed"});
    assert_eq!(
        verify_at(&card(&k, &payload_of(ISS, day(-1), None, b)), port).await,
        Err(Refusal::Payload("a bundle entry has no resource type"))
    );
    Ok(())
}

#[tokio::test]
async fn the_reason_the_keys_could_not_be_fetched_is_a_code() -> R {
    let k = test_key(7);
    let jws = card(&k, &payload(ISS, day(-1), None));
    for (paths, why) in [
        (vec![], "http_404"),
        (
            vec![("/.well-known/jwks.json".to_string(), "keys".to_string())],
            "not_json",
        ),
        (
            vec![("/.well-known/jwks.json".to_string(), "{}".to_string())],
            "not_jwks",
        ),
    ] {
        let (port, _) = serve(paths).await?;
        assert_eq!(verify_at(&jws, port).await, Err(Refusal::Keys(why.into())), "{why}");
    }
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0").await?.local_addr()?.port();
    assert_eq!(verify_at(&jws, closed).await, Err(Refusal::Keys("connect".into())));
    Ok(())
}

fn text_input(cards: Vec<String>) -> Input {
    Input {
        form: Form::Text,
        cards,
    }
}

#[tokio::test]
async fn cards_from_one_issuer_fetch_its_keys_and_revocation_list_once() -> R {
    let k = test_key(7);
    let mut jwk = k.jwk.clone();
    jwk["crlVersion"] = json!(1);
    let crl = json!({"kid": k.kid(), "method": "rid", "ctr": 1, "rids": ["Revoked1"]});
    let (port, hits) = site(json!({"keys": [jwk]}), Some((k.kid(), crl))).await?;
    let input = text_input(vec![with_rid(&k, "Card1"), with_rid(&k, "Card2")]);
    let got = shc::check(&input, &[HOST.to_string()], &local(port), now()).await;
    assert!(matches!(&got, Checked::Verified(cards) if cards.len() == 2), "{got:?}");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "one JWKS and one revocation list");
    Ok(())
}

#[tokio::test]
async fn the_whole_check_stops_at_its_time_limit() -> R {
    // a site that accepts and never answers
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let k = test_key(7);
    let input = text_input(vec![card(&k, &payload(ISS, day(-1), None))]);
    let started = Instant::now();
    let got = shc::check_within(
        &input,
        &[HOST.to_string()],
        &local(port),
        now(),
        Duration::from_millis(300),
    )
    .await;
    assert_eq!(
        got,
        Checked::Refused {
            refusal: Refusal::Keys("timeout".into()),
            iss: Some(ISS.into())
        }
    );
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    Ok(())
}

#[tokio::test]
async fn two_issuers_on_one_host_get_one_question() -> R {
    let k = test_key(7);
    let (a, b) = (format!("{ISS}/a"), format!("{ISS}/b"));
    let input = text_input(vec![
        card(&k, &payload(&a, day(-1), None)),
        card(&k, &payload(&b, day(-1), None)),
        card(&k, &payload(&a, day(-2), None)),
    ]);
    assert_eq!(
        shc::check(&input, &[], &shc::public, now()).await,
        Checked::NeedsApproval(vec![Ask {
            host: HOST.into(),
            issuers: vec![a, b]
        }])
    );
    Ok(())
}

#[tokio::test]
async fn a_literal_private_address_is_refused_before_the_person_is_asked() -> R {
    let k = test_key(7);
    for (iss, ip) in [("https://10.20.30.40", "10.20.30.40"), ("https://[::1]:8443", "::1")] {
        let input = text_input(vec![card(&k, &payload(iss, day(-1), None))]);
        assert_eq!(
            shc::check(&input, &[], &shc::public, now()).await,
            Checked::Refused {
                refusal: Refusal::PrivateIssuer(ip.into()),
                iss: None
            },
            "{iss}"
        );
    }
    // a name is never looked up before a yes: localhost is asked about like any host
    let input = text_input(vec![card(&k, &payload("https://localhost:8443", day(-1), None))]);
    assert_eq!(
        shc::check(&input, &[], &shc::public, now()).await,
        Checked::NeedsApproval(vec![Ask {
            host: "localhost:8443".into(),
            issuers: vec!["https://localhost:8443".into()]
        }])
    );
    Ok(())
}

async fn alex(tx: &mut sqlx::PgConnection) -> anyhow::Result<ownhealth_core::access::Profile> {
    let fam = family(&mut *tx).await?;
    Ok(get_profile(&mut *tx, fam.profile("alex")).await?.expect("alex"))
}

async fn import_text(
    tx: &mut sqlx::PgConnection,
    env: &Env,
    profile: &ownhealth_core::access::Profile,
    port: u16,
    text: &str,
    filename: Option<&str>,
) -> anyhow::Result<shc::Imported> {
    let input = shc::read(text.as_bytes())?;
    let approved = [HOST.to_string()];
    let received = Received {
        bytes: text.as_bytes(),
        filename,
        input: &input,
        approved: &approved,
    };
    match shc::receive(&mut *tx, &env.blobs, profile, &received, &local(port), now()).await? {
        Outcome::Imported(got) => Ok(got),
        other => anyhow::bail!("not imported: {other:?}"),
    }
}

#[tokio::test]
async fn a_card_for_another_person_says_whose_it_is_and_waits_in_review() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let jws = card(
        &k,
        &payload_of(ISS, day(-30), None, bundle_for("Carmen", "Ortega", "1955-01-30")),
    );
    let got = import_text(&mut tx, &env, &profile, port, &format!("shc:/{}", digits(&jws)), None).await?;
    assert_eq!(got.identity, "mismatch");
    assert_eq!(
        (got.cards[0].name.as_deref(), got.cards[0].birth_date.as_deref()),
        (Some("Carmen Ortega"), Some("1955-01-30"))
    );
    assert_eq!((got.summary.accepted, got.summary.queued), (0, 1));
    let reasons: Vec<String> =
        sqlx::query_scalar("select unnest(reasons) from review_item where document_id = $1 and kind = 'fhir_unmapped'")
            .bind(got.document_id)
            .fetch_all(&mut *tx)
            .await?;
    assert!(reasons.contains(&"identity_mismatch".to_string()), "{reasons:?}");
    Ok(())
}

#[tokio::test]
async fn a_card_document_names_its_signer_its_date_and_what_it_holds() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let first = format!("shc:/{}", digits(&card(&k, &payload(ISS, day(-30), None))));
    let got = import_text(&mut tx, &env, &profile, port, &first, None).await?;
    let (filename, provider, reported, entries): (Option<String>, Option<String>, Option<DateTime<Utc>>, Value) =
        sqlx::query_as(
            "select filename, provider_name, reported_at, flags -> 'shc' -> 0 -> 'entries' from document where id = $1",
        )
        .bind(got.document_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        filename.as_deref(),
        Some("SMART Health Card · issuer.labnorte.example · 2026-10-22")
    );
    assert_eq!(provider.as_deref(), Some(HOST));
    assert_eq!(reported.map(|t| t.timestamp()), Some(day(-30)));
    assert_eq!(
        entries,
        json!([{"type": "Immunization", "date": "2026-03-02", "text": "207"}])
    );
    let label: Option<String> = sqlx::query_scalar("select s.label from src s where s.document_id = $1")
        .bind(got.document_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        label.as_deref(),
        Some("SMART Health Card · issuer.labnorte.example · Immunization · 2026-03-02")
    );
    // another pasted card gets another name
    let mut later = payload(ISS, day(-10), None);
    later["vc"]["credentialSubject"]["fhirBundle"]["entry"][1]["resource"]["occurrenceDateTime"] = json!("2026-04-06");
    let second = format!("shc:/{}", digits(&card(&k, &later)));
    let other = import_text(&mut tx, &env, &profile, port, &second, None).await?;
    let name: Option<String> = sqlx::query_scalar("select filename from document where id = $1")
        .bind(other.document_id)
        .fetch_one(&mut *tx)
        .await?;
    assert_eq!(
        name.as_deref(),
        Some("SMART Health Card · issuer.labnorte.example · 2026-11-11")
    );
    Ok(())
}

#[tokio::test]
async fn the_same_card_in_another_form_adds_no_second_document() -> R {
    let env = Env::new().await?;
    let mut tx = env.tx().await?;
    let profile = alex(&mut tx).await?;
    let k = test_key(7);
    let (port, _) = site(json!({"keys": [k.jwk]}), None).await?;
    let jws = card(&k, &payload(ISS, day(-30), None));
    let first = import_text(&mut tx, &env, &profile, port, &format!("shc:/{}", digits(&jws)), None).await?;
    let file = json!({"verifiableCredential": [jws]}).to_string();
    let again = import_text(&mut tx, &env, &profile, port, &file, Some("alex.smart-health-card")).await?;
    assert_eq!((again.document_id, again.created), (first.document_id, false));
    assert_eq!((again.summary.accepted, again.summary.skipped), (0, 1));
    assert_eq!(documents(&mut tx, profile.id).await?, 1);
    Ok(())
}
