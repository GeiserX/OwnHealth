//! SMART Health Links, the sharing side: what a link carries and how its one file is sealed
//! (security-privacy.md section 11). The routes that serve a link are ownhealth-api `routes/shl.rs`.
//!
//! Spec: HL7 FHIR IG "SMART Health Cards and Links", Health Links Specification,
//! https://build.fhir.org/ig/HL7/smart-health-cards-and-links/links-specification.html. A comment
//! that quotes a section uses that page's heading.
//!
//! - The file is the profile's International Patient Summary ([`crate::ips::build`]) under the
//!   categories the link carries, so it says what the printed doctor summary says.
//! - The manifest id and the key are 32 random bytes each, base64url. Both go into the link and
//!   nowhere else: the server keeps the sha256 of the id ([`sha256`]) and never the key.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::NaiveDate;
use ownhealth_core::access::Scopes;
use ownhealth_core::error::Result;
use ownhealth_core::jose;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::ips::{self, Ips};

/// The categories a link can carry: what the IPS reads. `read:sensitive` and the genome scopes are
/// never among them, so a sensitive record or a genome finding never leaves through a link.
pub const SHAREABLE: [&str; 3] = ["read:labs", "read:records", "read:summary"];
/// "files array": FHIR, with the fhirVersion parameter servers SHOULD send.
pub const CONTENT_TYPE: &str = "application/fhir+json;fhirVersion=4.0.1";
/// "Establish a SMART Health Link Manifest URL": the url SHALL NOT exceed 128 characters.
pub const URL_MAX: usize = 128;
/// "Construct a SMART Health Link Payload": the label is no longer than 80 characters.
pub const LABEL_MAX: usize = 80;

/// 32 random bytes, base64url: 43 characters. "Establish a SMART Health Link Manifest URL": the
/// manifest URL SHALL include at least 256 bits of entropy.
pub fn random_segment() -> String {
    let mut b = [0u8; 32];
    rand::fill(&mut b[..]);
    URL_SAFE_NO_PAD.encode(b)
}

/// A manifest id or a file ticket as [`random_segment`] makes them.
pub fn is_segment(s: &str) -> bool {
    s.len() == 43 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

/// What the tables keep of a manifest id or a ticket.
pub fn sha256(segment: &str) -> Vec<u8> {
    Sha256::digest(segment.as_bytes()).to_vec()
}

/// The manifest URL of a link on the share host.
pub fn manifest_url(share_url: &str, id: &str) -> String {
    format!("{share_url}/api/shl/{id}")
}

/// A file location: the ticket a manifest answer made, under its link.
pub fn file_url(share_url: &str, id: &str, ticket: &str) -> String {
    format!("{share_url}/api/shl/{id}/file/{ticket}")
}

/// A sealed file and the key that opens it.
pub struct Sealed {
    /// base64url of the 32-byte key: for the link only.
    pub key: String,
    /// The compact JWE the instance stores and serves.
    pub jwe: String,
    pub ips: Ips,
}

impl std::fmt::Debug for Sealed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sealed")
            .field("jwe_len", &self.jwe.len())
            .finish_non_exhaustive()
    }
}

/// The IPS of `profile_id` under `scopes`, encrypted under a fresh key. "Encrypting and Decrypting
/// Files": JWE compact serialization, `alg: dir`, `enc: A256GCM`, a `cty` header naming the content
/// type, optional `zip: DEF`; jose::encrypt draws a new IV for every call.
pub async fn seal(
    conn: &mut PgConnection,
    profile_id: Uuid,
    scopes: &Scopes,
    lang: &str,
    as_of: NaiveDate,
) -> Result<Sealed> {
    let out = ips::build_link(conn, profile_id, scopes, lang, as_of).await?;
    let bytes = out.to_bytes()?;
    let mut key = [0u8; jose::KEY_LEN];
    rand::fill(&mut key[..]);
    let jwe = jose::encrypt(&key, &bytes, Some(CONTENT_TYPE), true)?;
    Ok(Sealed {
        key: URL_SAFE_NO_PAD.encode(key),
        jwe,
        ips: out,
    })
}

/// How many records a link of these categories leaves out because they are sensitive (mental
/// health, reproductive or sexual health): the current rows so tagged in the sections whose domain
/// the link carries. No link carries `read:sensitive`, so none of them is ever in the file; the
/// person creating the link sees the count, the recipient never does.
pub async fn sensitive_left_out(conn: &mut PgConnection, profile_id: Uuid, scopes: &Scopes) -> Result<i64> {
    let mut n = 0;
    for (table, domain) in ips::section_tables() {
        if !scopes.contains(&format!("read:{domain}")) {
            continue;
        }
        let sql = format!(
            "select count(*) from {table} where profile_id = $1 and cardinality(sensitivity) > 0 and {}",
            ownhealth_core::lifecycle::visible_sql("", "$2")
        );
        let c: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .bind(profile_id)
            .bind(None::<chrono::DateTime<chrono::Utc>>)
            .fetch_one(&mut *conn)
            .await?;
        n += c;
    }
    Ok(n)
}

/// "Construct a SMART Health Link Payload": `url`, `key`, `exp` (epoch seconds), `flag` (P when a
/// passcode guards the manifest) and `label`, minified, base64url, after `shlink:/`.
pub fn shlink(url: &str, key: &str, exp: i64, passcode: bool, label: Option<&str>) -> String {
    let mut p = Map::new();
    p.insert("url".into(), json!(url));
    p.insert("key".into(), json!(key));
    p.insert("exp".into(), json!(exp));
    if passcode {
        p.insert("flag".into(), json!("P"));
    }
    if let Some(l) = label {
        p.insert("label".into(), json!(l));
    }
    format!("shlink:/{}", URL_SAFE_NO_PAD.encode(Value::Object(p).to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_is_43_base64url_characters_of_256_random_bits() {
        let (a, b) = (random_segment(), random_segment());
        assert!(is_segment(&a) && is_segment(&b) && a != b);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).map(|v| v.len()).unwrap_or_default(), 32);
        assert!(!is_segment(&a[..42]) && !is_segment(&format!("{}=", &a[..42])));
    }

    #[test]
    fn the_payload_is_minified_base64url_json_with_the_p_flag_only_for_a_passcode() {
        let decode = |s: &str| -> Value {
            let b64 = s.strip_prefix("shlink:/").unwrap_or_default();
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(b64).unwrap_or_default()).unwrap_or_default()
        };
        let with = decode(&shlink(
            "https://share.example/api/shl/x",
            "k",
            1_800_000_000,
            true,
            Some("LabNorte"),
        ));
        assert_eq!(
            with,
            json!({"url": "https://share.example/api/shl/x", "key": "k", "exp": 1_800_000_000, "flag": "P", "label": "LabNorte"})
        );
        let without = decode(&shlink("https://share.example/api/shl/x", "k", 1, false, None));
        assert!(
            without.get("flag").is_none() && without.get("label").is_none(),
            "{without}"
        );
    }
}
