//! An offline ICE QR and an optional live SMART Health Link on a wallet PDF and a lock-screen PNG.

use std::collections::BTreeSet;
use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, NaiveDate, Utc};
use image::{ExtendedColorType, ImageEncoder as _, codecs::png::PngEncoder};
use ownhealth_core::access::today_utc;
use ownhealth_core::keys::{Dek, Keyring};
use ownhealth_core::langs;
use ownhealth_core::pdfwrite::{Font, Writer, text_width};
use ownhealth_core::{Error, Result, jose};
use ownhealth_ingest_pdf::pdfium::pdfium;
use pdfium_render::prelude::PdfRenderConfig;
use qrcode::{Color, EcLevel, QrCode, Version, bits::Bits};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;
use zeroize::Zeroizing;

pub const NOTICE: &str = "ice.access";

#[derive(Clone, Copy)]
struct Labels {
    qr_title: &'static str,
    sections: [&'static str; 3],
    contacts: &'static str,
    online: &'static str,
    offline: &'static str,
    expires: &'static str,
    title: &'static str,
}

const LABELS: &[(&str, Labels)] = &[
    (
        "es",
        Labels {
            qr_title: "Emergencia",
            sections: ["Alergias", "Enfermedades", "Medicamentos"],
            contacts: "Contactos de emergencia",
            online: "Resumen en línea",
            offline: "Información sin conexión",
            expires: "Caduca",
            title: "En caso de emergencia",
        },
    ),
    (
        "en",
        Labels {
            qr_title: "ICE",
            sections: ["Allergies", "Conditions", "Medicines"],
            contacts: "Emergency contacts",
            online: "Online summary",
            offline: "Offline information",
            expires: "Expires",
            title: "In case of emergency",
        },
    ),
];
pub const TOO_FULL: &str =
    "The emergency information is too long for a wallet-size QR code. Leave out some records and try again.";

pub fn missing_langs(languages: &[&str]) -> Vec<String> {
    langs::missing(&langs::codes(LABELS), languages)
        .into_iter()
        .map(|lang| format!("emergency card labels have no `{lang}` text"))
        .collect()
}

pub fn phrases() -> Value {
    let labels: Vec<_> = LABELS
        .iter()
        .map(|(_, l)| {
            json!([
                l.qr_title, l.sections, l.contacts, l.online, l.offline, l.expires, l.title
            ])
        })
        .collect();
    json!([labels, TOO_FULL])
}

#[derive(sqlx::FromRow)]
pub struct Item {
    pub id: Uuid,
    pub section: i32,
    pub name: String,
    pub src_id: String,
    pub sensitive: bool,
}

pub const KINDS: [&str; 3] = ["allergy", "condition", "medicine"];

/// Read only the fields that may go onto the card. The outer filter also keeps the names and IDs
/// of sensitive records out of the options response when the owner cannot share them.
pub async fn items(conn: &mut PgConnection, profile: Uuid, may_pick: bool) -> Result<Vec<Item>> {
    Ok(sqlx::query_as(
        "select id, section, name, src_id, cardinality(sensitivity) > 0 as sensitive from (
           select id, 0 as section, substance as name, src_id, sensitivity from allergy
           where profile_id = $1 and status = 'current' and clinical_status not in ('inactive', 'resolved')
             and verification_status not in ('refuted', 'entered-in-error')
             and (effective_at is null or (effective_at at time zone 'UTC')::date <= $2)
           union all
           select id, 1, text, src_id, sensitivity from condition
           where profile_id = $1 and status = 'current' and clinical_status in ('active', 'recurrence', 'relapse')
             and verification_status not in ('refuted', 'entered-in-error')
             and (onset is null or onset <= $2) and (effective_at is null or (effective_at at time zone 'UTC')::date <= $2)
           union all
           select id, 2, concat_ws(' ', name, nullif(strength, '')), src_id, sensitivity from medication_statement
           where profile_id = $1 and status = 'current' and kind = 'medication' and med_status = 'active'
             and (started is null or started <= $2) and (stopped is null or stopped >= $2)
         ) facts where $3 or cardinality(sensitivity) = 0 order by section, name collate \"C\", id",
    )
    .bind(profile)
    .bind(today_utc())
    .bind(may_pick)
    .fetch_all(conn)
    .await?)
}

/// Control characters, and the bidirectional marks, embeddings, overrides and isolates (Unicode
/// category Cf) that would reorder the lines a phone shows after a scan.
pub fn hidden(c: char) -> bool {
    c.is_control()
        || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// The text with whitespace runs made single spaces and hidden characters left out.
pub fn words(text: &str) -> String {
    text.split_whitespace()
        .map(|word| word.chars().filter(|c| !hidden(*c)).collect::<String>())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn text(lang: &str, items: &[&Item], contacts: &[String]) -> String {
    let labels = langs::pick(LABELS, lang);
    let mut lines = vec![labels.qr_title.to_owned()];
    for (section, label) in labels.sections.iter().enumerate() {
        let names: BTreeSet<String> = items
            .iter()
            .filter(|i| i.section == section as i32)
            .map(|i| words(&i.name))
            .filter(|name| !name.is_empty())
            .collect();
        if !names.is_empty() {
            let list = names.into_iter().collect::<Vec<_>>().join("; ");
            lines.push(format!("{label}: {list}"));
        }
    }
    if !contacts.is_empty() {
        lines.push(format!("{}: {}", labels.contacts, contacts.join("; ")));
    }
    lines.join("\n")
}

/// Version 20 keeps modules large enough on a wallet print, including the four-module quiet zone.
/// Use the UTF-8 ECI so that scanners can read Spanish text without guessing its encoding.
fn qr(text: &str) -> Result<QrCode> {
    for version in 1..=20 {
        let mut bits = Bits::new(Version::Normal(version));
        if bits.push_eci_designator(26).is_ok()
            && bits.push_optimal_data(text.as_bytes()).is_ok()
            && bits.push_terminator(EcLevel::M).is_ok()
            && let Ok(code) = QrCode::with_bits(bits, EcLevel::M)
        {
            return Ok(code);
        }
    }
    Err(Error::Invalid(TOO_FULL.into()))
}

fn draw_qr(writer: &mut Writer, code: &QrCode, x: f64, y: f64, side: f64) {
    let n = code.width();
    let module = side / (n + 8) as f64;
    for row in 0..n {
        let mut col = 0;
        while col < n {
            if code[(col, row)] == Color::Light {
                col += 1;
                continue;
            }
            let start = col;
            while col < n && code[(col, row)] == Color::Dark {
                col += 1;
            }
            writer.fill_rect(
                x + (start + 4) as f64 * module,
                y + (n + 3 - row) as f64 * module,
                (col - start) as f64 * module,
                module,
            );
        }
    }
}

fn centered(writer: &mut Writer, text: &str, x: f64, y: f64, font: Font, size: f64) {
    writer.text(x - text_width(text, font, size) / 2.0, y, text, font, size);
}

fn layout(lang: &str, date: NaiveDate, offline: &QrCode, online: Option<&QrCode>, phone: bool) -> Vec<u8> {
    let labels = langs::pick(LABELS, lang);
    let (width, height) = if phone { (270.0, 480.0) } else { (243.0, 153.0) };
    let mid = width / 2.0;
    let mut w = Writer::sized(width, height, 0.0);
    let (head, foot, title_size, caption_size) = if phone {
        (358.0, 21.0, 19.0, 11.0)
    } else {
        (137.0, 8.0, 12.0, 8.0)
    };
    let expiry = format!("{} {date} 00:00 UTC", labels.expires);
    centered(&mut w, labels.title, mid, head, Font::Bold, title_size);
    centered(&mut w, &expiry, mid, foot, Font::Regular, caption_size);
    let (side, offline_x, offline_y, online_x, online_y) = match (phone, online.is_some()) {
        (false, false) => (101.0, mid, 27.0, mid, 27.0),
        (false, true) => (101.0, 65.0, 27.0, 178.0, 27.0),
        (true, false) => (169.0, mid, 129.0, mid, 129.0),
        (true, true) => (130.0, mid, 210.0, mid, 52.0),
    };
    for (code, x, y, label) in [
        (Some(offline), offline_x, offline_y, labels.offline),
        (online, online_x, online_y, labels.online),
    ] {
        if let Some(code) = code {
            draw_qr(&mut w, code, x - side / 2.0, y, side);
            centered(&mut w, label, x, y - caption_size, Font::Bold, caption_size);
        }
    }
    w.finish().pdf
}

pub struct Files {
    pub pdf: Vec<u8>,
    pub png: Vec<u8>,
}

/// The PNG is a portrait page from the same deterministic writer, rendered by the app's PDFium.
pub fn render(lang: &str, date: NaiveDate, plain: &str, live: Option<&str>) -> Result<Files> {
    let offline = qr(plain)?;
    let online = live.map(qr).transpose()?;
    let pdf = layout(lang, date, &offline, online.as_ref(), false);
    let portrait = layout(lang, date, &offline, online.as_ref(), true);
    let png = (|| -> anyhow::Result<Vec<u8>> {
        let doc = pdfium()?.load_pdf_from_byte_slice(&portrait, None)?;
        let page = doc.pages().get(0)?;
        let bitmap = page.render_with_config(&PdfRenderConfig::new().set_target_width(1080))?;
        let mut png = Vec::new();
        PngEncoder::new(&mut png).write_image(
            &bitmap.as_rgba_bytes(),
            bitmap.width() as u32,
            bitmap.height() as u32,
            ExtendedColorType::Rgba8,
        )?;
        Ok(png)
    })()?;
    Ok(Files { pdf, png })
}

/// Record a delivered online file in the notification outbox for each owner of the profile.
pub async fn notify_access(conn: &mut PgConnection, profile: Uuid) -> Result<()> {
    sqlx::query(
        "insert into notification_outbox (user_id, profile_id, kind)
         select user_id, profile_id, $2 from profile_grant where profile_id = $1
           and role = 'owner' and not break_glass and (expires_at is null or expires_at > clock_timestamp())",
    )
    .bind(profile)
    .bind(NOTICE)
    .execute(conn)
    .await?;
    Ok(())
}

/// The card key belongs only to this card's row and profile.
fn key_aad(profile: Uuid, id: Uuid) -> Vec<u8> {
    [b"ownhealth.ice.v1".as_slice(), profile.as_bytes(), id.as_bytes()].concat()
}

pub async fn dek(keys: Option<Arc<Keyring>>, profile: Uuid, create: bool) -> Result<Arc<Dek>> {
    let keys = keys.ok_or_else(|| Error::Other(anyhow::anyhow!("ICE needs a profile key")))?;
    tokio::task::spawn_blocking(move || keys.dek(profile, create))
        .await
        .map_err(|e| anyhow::anyhow!("ICE key lookup failed: {e}"))??
        .ok_or_else(|| Error::Other(anyhow::anyhow!("The ICE profile key is unavailable")))
}

pub struct Online {
    pub qr: String,
    pub sealed_key: Vec<u8>,
}

pub fn online(dek: &Dek, profile: Uuid, id: Uuid, url: &str, expires: DateTime<Utc>) -> Result<Online> {
    let mut key = Zeroizing::new([0; jose::KEY_LEN]);
    rand::fill(key.as_mut_slice());
    let sealed_key = dek.seal_value(key.as_slice(), &key_aad(profile, id))?;
    let payload =
        json!({"url":url, "key":URL_SAFE_NO_PAD.encode(key.as_slice()), "exp":expires.timestamp(), "flag":"LU"});
    let qr = format!("shlink:/{}", URL_SAFE_NO_PAD.encode(payload.to_string()));
    Ok(Online { qr, sealed_key })
}

pub fn encrypt(dek: &Dek, profile: Uuid, id: Uuid, sealed_key: &[u8], content: &[u8]) -> Result<String> {
    let key = dek.open_value(sealed_key, &key_aad(profile, id))?;
    let key = key
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid ICE key length"))?;
    Ok(jose::encrypt(key, content, Some(crate::shl::CONTENT_TYPE), true)?)
}
