//! The wallet card through the same routes the owner and a responder use.
#[path = "auth_support.rs"]
mod support;

use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ownhealth_core::access::{scope_set, set_grant, set_locks, today_utc};
use ownhealth_core::jose;
use ownhealth_core::provenance::{SrcIn, SrcRef};
use ownhealth_core::records::{RecordIn, RecordOptions, add_record};
use serde_json::{Value, json};
use support::{Client, Env};
use uuid::Uuid;

type R = anyhow::Result<()>;
const CARD: &str = "/api/profiles/alex/ice";
const SHARE: &str = "https://share.rivera.test";
const PRIVATE: &str = "Rivera confidential condition";

async fn condition(env: &Env, text: &str, sensitivity: &[&str]) -> anyhow::Result<Uuid> {
    let profile = env.profile("alex");
    let record = RecordIn::from_json(
        "condition",
        json!({"profile_id": profile, "text": text, "verification_status": "confirmed", "sensitivity": sensitivity}),
    )?;
    let src = SrcRef::New(SrcIn {
        profile_id: Some(profile),
        ..SrcIn::manual(None)
    });
    let mut c = env.pool().acquire().await?;
    add_record(&mut c, &record, &src, RecordOptions::new("person")).await?;
    Ok(
        sqlx::query_scalar("select id from condition where profile_id = $1 and text = $2")
            .bind(profile)
            .bind(text)
            .fetch_one(&mut *c)
            .await?,
    )
}

async fn emergency_facts(env: &Env) -> R {
    let profile = env.profile("alex");
    let src = SrcRef::New(SrcIn {
        profile_id: Some(profile),
        ..SrcIn::manual(None)
    });
    let mut c = env.pool().acquire().await?;
    for (kind, fields) in [
        ("allergy", json!({"profile_id": profile, "substance": "Penicillin"})),
        (
            "medication",
            json!({"profile_id": profile, "name": "Cetirizine", "strength": "10 mg"}),
        ),
        (
            "condition",
            json!({"profile_id": profile, "text": "Old injury", "clinical_status": "resolved"}),
        ),
    ] {
        let record = RecordIn::from_json(kind, fields)?;
        add_record(&mut c, &record, &src, RecordOptions::new("person")).await?;
    }
    Ok(())
}

async fn record(env: &Env, kind: &str, mut fields: Value) -> R {
    let profile = env.profile("alex");
    fields["profile_id"] = json!(profile);
    let record = RecordIn::from_json(kind, fields)?;
    let src = SrcRef::New(SrcIn {
        profile_id: Some(profile),
        ..SrcIn::manual(None)
    });
    let mut c = env.pool().acquire().await?;
    add_record(&mut c, &record, &src, RecordOptions::new("person")).await?;
    Ok(())
}

async fn issue(client: &mut Client, body: Value) -> anyhow::Result<Value> {
    let r = client.post(CARD, &body).await?;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.header("cache-control"), "no-store");
    Ok(r.json)
}

fn scans(card: &Value) -> anyhow::Result<Vec<String>> {
    let bytes = STANDARD.decode(card["png"].as_str().unwrap_or_default())?;
    let image = image::load_from_memory(&bytes)?.into_luma8();
    scan_image(&image)
}

fn paper_scans(card: &Value) -> anyhow::Result<Vec<String>> {
    let bytes = STANDARD.decode(card["pdf"].as_str().unwrap_or_default())?;
    let pdfium = ownhealth_ingest_pdf::pdfium::pdfium()?;
    let doc = pdfium.load_pdf_from_byte_slice(&bytes, None)?;
    assert_eq!(doc.pages().len(), 1);
    let page = doc.pages().get(0)?;
    assert_eq!(page.width().value, 243.0);
    assert_eq!(page.height().value, 153.0);
    let bitmap = page.render(972, 612, None)?;
    let rgba = image::RgbaImage::from_raw(972, 612, bitmap.as_rgba_bytes())
        .ok_or_else(|| anyhow::anyhow!("unexpected PDF bitmap dimensions"))?;
    scan_image(&image::DynamicImage::ImageRgba8(rgba).into_luma8())
}

fn scan_image(image: &image::GrayImage) -> anyhow::Result<Vec<String>> {
    let mut hints = rxing::DecodeHints::default()
        .with(rxing::DecodeHintValue::TryHarder(true))
        .with(rxing::DecodeHintValue::PossibleFormats(
            std::collections::HashSet::from([rxing::BarcodeFormat::QR_CODE]),
        ));
    let found = rxing::helpers::detect_multiple_in_luma_with_hints(
        image.as_raw().clone(),
        image.width(),
        image.height(),
        &mut hints,
    )?;
    Ok(found.into_iter().map(|qr| qr.getText().to_string()).collect())
}

fn link(card: &Value) -> anyhow::Result<Value> {
    let scans = paper_scans(card)?;
    let encoded = scans
        .iter()
        .find_map(|text| text.strip_prefix("shlink:/"))
        .ok_or_else(|| anyhow::anyhow!("the card has no SMART Health Link QR"))?;
    Ok(serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded)?)?)
}

fn direct(payload: &Value) -> anyhow::Result<String> {
    let url = payload["url"]
        .as_str()
        .and_then(|url| url.strip_prefix(SHARE))
        .ok_or_else(|| anyhow::anyhow!("unexpected share host"))?;
    Ok(format!("{url}?recipient=LabNorte"))
}

#[tokio::test]
async fn identical_offline_contents_render_identical_pdf_and_png() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let body = json!({"lang": "en", "days": 7});
    let a = issue(&mut alex, body.clone()).await?;
    let b = issue(&mut alex, body).await?;
    assert!(
        STANDARD
            .decode(a["pdf"].as_str().unwrap_or_default())?
            .starts_with(b"%PDF-")
    );
    assert!(
        STANDARD
            .decode(a["png"].as_str().unwrap_or_default())?
            .starts_with(b"\x89PNG")
    );
    assert!(a["pdf"] == b["pdf"], "PDF bytes differ");
    assert!(a["png"] == b["png"], "PNG bytes differ");
    Ok(())
}

#[tokio::test]
async fn offline_qr_carries_only_minimal_text_and_selected_sensitive_items() -> R {
    let env = Env::new(true).await?;
    emergency_facts(&env).await?;
    condition(&env, "Asthma", &[]).await?;
    let private = condition(&env, PRIVATE, &["mental_health"]).await?;
    condition(&env, "Second private condition", &["mental_health"]).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let contacts = json!([{"name": "Carmen Ortega", "phone": "+34 555 010 020"}]);
    let card = issue(&mut alex, json!({"lang": "en", "contacts": contacts})).await?;
    let offline = scans(&card)?;
    assert_eq!(offline.len(), 1);
    assert_eq!(
        offline[0],
        "ICE\nAllergies: Penicillin\nConditions: Asthma\nMedicines: Cetirizine 10 mg\nEmergency contacts: Carmen Ortega +34 555 010 020"
    );
    assert_eq!(paper_scans(&card)?, offline);
    let picked = issue(&mut alex, json!({"lang":"en","sensitive_ids":[private]})).await?;
    let picked = scans(&picked)?;
    assert!(picked.iter().any(|qr| qr.contains(PRIVATE)));
    assert!(picked.iter().all(|qr| !qr.contains("Second private condition")));
    Ok(())
}

#[tokio::test]
async fn printed_online_qr_reads_updated_file_with_the_same_key_and_stops_on_revoke() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"lang":"en","online":true})).await?;
    let printed = link(&card)?;
    assert_eq!(printed["flag"], "LU");
    let raw = URL_SAFE_NO_PAD.decode(printed["key"].as_str().unwrap_or_default())?;
    let key: [u8; jose::KEY_LEN] = raw.try_into().map_err(|_| anyhow::anyhow!("wrong key length"))?;
    let path = direct(&printed)?;
    let mut outside = env.anon();
    let first = outside.get(&path).await?;
    assert_eq!(first.status, StatusCode::OK);
    let open = |bytes: Vec<u8>| -> anyhow::Result<String> {
        let file = jose::decrypt(&key, &String::from_utf8(bytes)?, 1 << 24)?;
        Ok(String::from_utf8(file.plaintext)?)
    };
    assert!(!open(first.bytes)?.contains("New public condition"));
    condition(&env, "New public condition", &[]).await?;
    condition(&env, PRIVATE, &["mental_health"]).await?;
    let updated = outside.get(&path).await?;
    assert_eq!(updated.status, StatusCode::OK);
    let updated = open(updated.bytes)?;
    assert!(updated.contains("New public condition"));
    assert!(!updated.contains(PRIVATE));
    let id = card["id"].as_str().unwrap_or_default();
    let list = alex.get("/api/profiles/alex/shl").await?;
    assert_eq!(list.json["items"][0]["ice"], true);
    assert_eq!(list.json["items"][0]["id"], id);
    assert_eq!(
        alex.delete(&format!("/api/profiles/alex/shl/{id}")).await?.status,
        StatusCode::OK
    );
    assert_eq!(outside.get(&path).await?.status, StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn every_delivered_file_writes_an_owner_notification() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"online":true})).await?;
    let path = direct(&link(&card)?)?;
    let mut outside = env.anon();
    for _ in 0..2 {
        assert_eq!(outside.get(&path).await?.status, StatusCode::OK);
    }
    let n: i64 = sqlx::query_scalar(
        "select count(*) from notification_outbox where user_id = $1 and profile_id = $2 and kind = 'ice.access'",
    )
    .bind(env.user("alex"))
    .bind(env.profile("alex"))
    .fetch_one(env.pool())
    .await?;
    assert_eq!(n, 2);
    Ok(())
}

#[tokio::test]
async fn reissue_retires_the_old_qr_and_expiry_erases_the_new_sealed_key() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let first = issue(&mut alex, json!({"online":true})).await?;
    let first = link(&first)?;
    let second = issue(&mut alex, json!({"online":true})).await?;
    let second_id = Uuid::parse_str(second["id"].as_str().unwrap_or_default())?;
    let second = link(&second)?;
    assert!(first["key"] != second["key"], "reissue carried forward an old key");
    let mut outside = env.anon();
    assert_eq!(outside.get(&direct(&first)?).await?.status, StatusCode::NOT_FOUND);
    assert_eq!(outside.get(&direct(&second)?).await?.status, StatusCode::OK);
    let mut c = env.pool().acquire().await?;
    sqlx::query("update shl set expires_at = now() - interval '1 second' where id = $1")
        .bind(second_id)
        .execute(&mut *c)
        .await?;
    assert_eq!(outside.get(&direct(&second)?).await?.status, StatusCode::NOT_FOUND);
    ownhealth_core::sources::sweep_link_files(&mut c).await?;
    let n: i64 = sqlx::query_scalar("select count(*) from shl_file")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(n, 0);
    Ok(())
}

#[tokio::test]
async fn an_owner_needs_step_up_and_cannot_pick_a_locked_sensitive_record() -> R {
    let env = Env::new(true).await?;
    let private = condition(&env, PRIVATE, &["mental_health"]).await?;
    let mut stale = env.stale(env.user("alex")).await?;
    assert_eq!(stale.post(CARD, &json!({})).await?.code(), "step_up_required");
    let mut alex = env.as_user(env.user("alex")).await?;
    assert_eq!(alex.get(CARD).await?.json["items"][0]["id"], private.to_string());
    let mut c = env.pool().acquire().await?;
    ownhealth_core::access::set_locks(
        &mut c,
        env.profile("alex"),
        &ownhealth_core::access::scope_set(["read:sensitive"]),
    )
    .await?;
    assert_eq!(alex.get(CARD).await?.json["items"], json!([]));
    let denied = alex.post(CARD, &json!({"sensitive_ids":[private]})).await?;
    assert_eq!(denied.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(denied.code(), "ice_picks");
    Ok(())
}

#[tokio::test]
async fn only_the_owner_can_read_the_options_or_issue_a_card() -> R {
    let env = Env::new(true).await?;
    // alex is lucia's guardian and carmen's caregiver; sam only views alex
    let mut alex = env.as_user(env.user("alex")).await?;
    for slug in ["lucia", "carmen"] {
        let path = format!("/api/profiles/{slug}/ice");
        assert_eq!(alex.get(&path).await?.code(), "ice_owner", "GET {slug}");
        let made = alex.post(&path, &json!({})).await?;
        assert_eq!(made.status, StatusCode::FORBIDDEN, "POST {slug}");
        assert_eq!(made.code(), "ice_owner");
    }
    let mut sam = env.as_user(env.user("sam")).await?;
    assert_eq!(sam.get(CARD).await?.status, StatusCode::FORBIDDEN);
    assert_eq!(sam.post(CARD, &json!({})).await?.status, StatusCode::FORBIDDEN);
    Ok(())
}

#[tokio::test]
async fn a_lock_on_records_or_summary_closes_the_card() -> R {
    let env = Env::new(true).await?;
    record(&env, "allergy", json!({"substance": "Penicillin"})).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    for lock in ["read:records", "read:summary"] {
        let mut c = env.pool().acquire().await?;
        set_locks(&mut c, env.profile("alex"), &scope_set([lock])).await?;
        drop(c);
        let options = alex.get(CARD).await?;
        assert_eq!(options.status, StatusCode::FORBIDDEN, "GET with {lock} locked");
        assert_eq!(options.code(), "ice_closed");
        let made = alex.post(CARD, &json!({"lang": "en"})).await?;
        assert_eq!(made.status, StatusCode::FORBIDDEN, "POST with {lock} locked");
        assert_eq!(made.code(), "ice_closed");
        assert!(made.json.get("pdf").is_none() && made.json.get("png").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn losing_ownership_ends_the_card_and_erases_its_key() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"online": true})).await?;
    let path = direct(&link(&card)?)?;
    let mut outside = env.anon();
    assert_eq!(outside.get(&path).await?.status, StatusCode::OK);
    let mut c = env.pool().acquire().await?;
    set_grant(
        &mut c,
        env.user("alex"),
        env.profile("alex"),
        "guardian",
        None,
        None,
        None,
    )
    .await?;
    assert_eq!(outside.get(&path).await?.status, StatusCode::NOT_FOUND);
    let files: i64 = sqlx::query_scalar("select count(*) from shl_file")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(files, 0, "the sealed card key outlived the owner's grant");
    let reason: Option<String> = sqlx::query_scalar("select revoked_reason from shl where flag = 'LU'")
        .fetch_one(&mut *c)
        .await?;
    assert_eq!(reason.as_deref(), Some("access_changed"));
    Ok(())
}

#[tokio::test]
async fn a_guardian_of_the_profile_cannot_see_revoke_or_hear_of_the_card() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut c = env.pool().acquire().await?;
    set_grant(
        &mut c,
        env.user("sam"),
        env.profile("alex"),
        "guardian",
        None,
        None,
        None,
    )
    .await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"online": true})).await?;
    let id = card["id"].as_str().unwrap_or_default().to_owned();
    let path = direct(&link(&card)?)?;
    let mut outside = env.anon();
    assert_eq!(outside.get(&path).await?.status, StatusCode::OK);
    let mut sam = env.as_user(env.user("sam")).await?;
    let list = sam.get("/api/profiles/alex/shl").await?;
    assert_eq!(list.status, StatusCode::OK);
    let items = list.json["items"].as_array().cloned().unwrap_or_default();
    assert!(
        items.iter().all(|item| item["id"] != id.as_str()),
        "a guardian listed the card"
    );
    let log = sam.get(&format!("/api/profiles/alex/shl/{id}/access")).await?;
    assert!(log.json["items"].as_array().is_none_or(Vec::is_empty), "{}", log.json);
    assert_eq!(
        sam.delete(&format!("/api/profiles/alex/shl/{id}")).await?.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(outside.get(&path).await?.status, StatusCode::OK);
    let notices: i64 =
        sqlx::query_scalar("select count(*) from notification_outbox where user_id = $1 and kind = 'ice.access'")
            .bind(env.user("sam"))
            .fetch_one(&mut *c)
            .await?;
    assert_eq!(notices, 0);
    Ok(())
}

#[tokio::test]
async fn stopped_refuted_and_inactive_records_stay_off_the_card() -> R {
    let env = Env::new(true).await?;
    for (kind, fields) in [
        ("medication", json!({"name": "Warfarin", "stopped": "2025-01-01"})),
        ("medication", json!({"name": "Digoxin", "med_status": "stopped"})),
        (
            "allergy",
            json!({"substance": "Latex", "verification_status": "refuted"}),
        ),
        ("allergy", json!({"substance": "Peanut", "clinical_status": "inactive"})),
        ("allergy", json!({"substance": "Penicillin"})),
    ] {
        record(&env, kind, fields).await?;
    }
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"lang": "en"})).await?;
    assert_eq!(scans(&card)?, vec!["ICE\nAllergies: Penicillin".to_owned()]);
    Ok(())
}

#[tokio::test]
async fn the_card_expires_at_00_00_utc_after_the_chosen_days() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"days": 1})).await?;
    let tomorrow = today_utc() + chrono::Days::new(1);
    assert_eq!(card["expires_at"], json!(format!("{tomorrow}T00:00:00Z")));
    Ok(())
}

#[tokio::test]
async fn reissuing_an_offline_card_marks_the_first_one_replaced() -> R {
    let env = Env::new(true).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let first = issue(&mut alex, json!({"lang": "en"})).await?;
    let second = issue(&mut alex, json!({"lang": "en"})).await?;
    let list = alex.get("/api/profiles/alex/shl").await?;
    let items = list.json["items"].as_array().cloned().unwrap_or_default();
    let of = |card: &Value| {
        items
            .iter()
            .find(|item| item["id"] == card["id"])
            .cloned()
            .unwrap_or(Value::Null)
    };
    assert_eq!(of(&second)["state"], "active");
    assert_eq!(
        of(&first)["state"],
        "revoked",
        "the replaced offline card stayed active"
    );
    assert_eq!(of(&first)["reason"], "replaced");
    Ok(())
}

#[tokio::test]
async fn a_malformed_direct_query_answers_with_cors_and_no_store() -> R {
    let mut env = Env::new(true).await?;
    env.settings.share_url = Some(SHARE.into());
    let mut alex = env.as_user(env.user("alex")).await?;
    let card = issue(&mut alex, json!({"online": true})).await?;
    let base = direct(&link(&card)?)?;
    let base = base.split('?').next().unwrap_or_default().to_owned();
    let mut outside = env.anon();
    for query in ["?recipient=%ZZ&recipient=x", "?recipient=a&recipient=b", ""] {
        let r = outside.get(&format!("{base}{query}")).await?;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{query}");
        assert_eq!(r.code(), "bad_request", "{query}");
        assert_eq!(r.header("access-control-allow-origin"), "*", "{query}");
        assert_eq!(r.header("cache-control"), "no-store", "{query}");
    }
    Ok(())
}

#[tokio::test]
async fn bidi_controls_never_reach_the_offline_qr() -> R {
    let env = Env::new(true).await?;
    record(&env, "allergy", json!({"substance": "Penicillin\u{202e}"})).await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let contacts = json!([{"name": "Ana\u{2067} Ortega\u{200f}", "phone": "+34 555 010 020"}]);
    let card = issue(&mut alex, json!({"lang": "en", "contacts": contacts})).await?;
    assert_eq!(
        scans(&card)?,
        vec!["ICE\nAllergies: Penicillin\nEmergency contacts: Ana Ortega +34 555 010 020".to_owned()]
    );
    Ok(())
}

#[tokio::test]
async fn a_record_too_long_for_one_qr_fits_once_the_owner_leaves_items_out() -> R {
    let env = Env::new(true).await?;
    for i in 0..40 {
        record(
            &env,
            "medication",
            json!({"name": format!("Medicine number {i:02}"), "strength": "10 mg"}),
        )
        .await?;
    }
    let mut alex = env.as_user(env.user("alex")).await?;
    let full = alex.post(CARD, &json!({"lang": "en"})).await?;
    assert_eq!(full.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(full.code(), "ice_full");
    let options = alex.get(CARD).await?;
    let items = options.json["items"].as_array().cloned().unwrap_or_default();
    assert_eq!(items.len(), 40, "every medicine is offered: {}", options.json);
    assert!(
        items
            .iter()
            .all(|item| item["sensitive"] == false && item["kind"] == "medicine")
    );
    let left_out: Vec<Value> = items[20..].iter().map(|item| item["id"].clone()).collect();
    let card = issue(&mut alex, json!({"lang": "en", "left_out": left_out})).await?;
    let text = scans(&card)?.join("\n");
    assert!(text.contains("Medicine number 19 10 mg"), "{text}");
    assert!(!text.contains("Medicine number 20"), "{text}");
    Ok(())
}

#[tokio::test]
async fn card_opens_are_one_notice_entry_per_profile_with_their_count() -> R {
    let env = Env::new(true).await?;
    // 250 opens of the card, all unread, older than any other notice
    sqlx::query(
        "insert into notification_outbox (user_id, profile_id, kind, created_at)
         select $1, $2, 'ice.access', now() - make_interval(secs => n) from generate_series(1, 250) n",
    )
    .bind(env.user("alex"))
    .bind(env.profile("alex"))
    .execute(env.pool())
    .await?;
    let mut alex = env.as_user(env.user("alex")).await?;
    let seen = alex.get("/api/notifications").await?;
    assert_eq!(seen.status, StatusCode::OK);
    // the list keeps its 200 newest; the older unread opens do not flood it
    assert_eq!(seen.json["items"].as_array().map(Vec::len), Some(200));
    let ice = seen.json["ice"].as_array().cloned().unwrap_or_default();
    assert_eq!(ice.len(), 1, "{}", seen.json["ice"]);
    assert_eq!(ice[0]["profile"]["slug"], "alex");
    assert_eq!(ice[0]["count"], 250);
    assert_eq!(ice[0]["unread"], 250);
    assert!(ice[0]["last_at"].is_string());
    Ok(())
}
