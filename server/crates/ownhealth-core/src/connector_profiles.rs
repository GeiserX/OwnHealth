//! The connection profiles (`data/connectors/connectors.yaml`): per US vendor, what a self-hoster
//! registers at the vendor (client type, redirect URI, scopes, the registration page) and how the
//! vendor's tokens behave. The bring-your-own client wizard shows the first half; the second half
//! drives the token code in [`crate::connector_oauth`]:
//!
//! - [`Rotation::Rotating`]: a refresh answer without a new refresh token retired the old one, so the
//!   old one is dropped and never sent again; the connection asks for consent when its access token ends.
//! - [`Rotation::SingleUse`]: the refresh takes the careful path that marks the connection before the
//!   token leaves, as for Blue Button.
//! - `unused_expiry_days`: the sign-in ends that many days after the last refresh, unless the vendor's
//!   own answer names `refresh_token_expires_in`.
//! - `access_token_seconds`: the access token's life when the answer leaves out `expires_in`.
//!
//! A household client whose vendor the file does not name keeps the plain behaviour: a new refresh
//! token replaces the old one, and the answer's own times count.
//!
//! The file is compiled in. The server checks it at start ([`check`]) and refuses to start on a bad
//! file, naming the line.

use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::connector_oauth::{self as oauth, BLUEBUTTON_SCOPES, SMART_SCOPES};

/// The file as the repository holds it, for error messages.
pub const FILE: &str = "data/connectors/connectors.yaml";

const SHIPPED: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../../data/connectors/connectors.yaml"
));

/// How the vendor authenticates the app at its token endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientType {
    /// A client id and a client secret; the secret serves only the server it was saved for.
    Confidential,
    /// A client id alone, for every server of the vendor.
    Public,
}

/// What a refresh does to the refresh token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rotation {
    /// The refresh token stays the same.
    None,
    /// Each answer brings a new refresh token and retires the old one.
    Rotating,
    /// Rotating, and a token sent once is spent even when no answer comes back.
    SingleUse,
    /// Not documented: a new token in an answer replaces the old one.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Refresh {
    pub rotation: Rotation,
    /// The refresh token ends after this many days without a refresh.
    pub unused_expiry_days: Option<u32>,
}

/// One vendor's profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vendor {
    /// The map key: `connector_client.vendor`, and `fhir_brand.vendor` for a directory vendor.
    #[serde(skip_deserializing, default)]
    pub key: String,
    pub name: String,
    /// The source kind a connection makes (`smart`, `bluebutton`).
    pub kind: String,
    pub client_type: ClientType,
    /// The vendor's fixed FHIR servers.
    #[serde(default)]
    pub servers: Vec<String>,
    /// The US provider directory lists the vendor's servers.
    pub directory: bool,
    pub registration: String,
    pub scopes: Vec<String>,
    pub refresh: Refresh,
    pub access_token_seconds: Option<u32>,
    pub verified_live: bool,
    #[serde(default)]
    pub sources: Vec<String>,
}

/// The whole file, vendors in file order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profiles {
    pub version: u32,
    pub redirect_path: String,
    pub vendors: Vec<Vendor>,
}

impl Profiles {
    pub fn get(&self, key: &str) -> Option<&Vendor> {
        self.vendors.iter().find(|v| v.key == key)
    }

    /// The redirect URI to register: the instance's address and the callback route.
    pub fn redirect_uri(&self, base_url: &str) -> String {
        format!("{}{}", base_url.trim_end_matches('/'), self.redirect_path)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    version: u32,
    redirect_path: String,
    vendors: serde_yaml::Mapping,
}

/// The connector routes' callback, which every vendor sends the browser back to.
pub const CALLBACK_PATH: &str = "/api/connectors/callback";

/// The one-based line of `vendors:` entry `key`, or of the first line inside it that holds `needle`.
fn line_of(text: &str, key: &str, needle: Option<&str>) -> Option<usize> {
    let lines: Vec<&str> = text.lines().collect();
    let head = format!("  {key}:");
    let start = lines.iter().position(|l| l.trim_end() == head)?;
    if let Some(n) = needle.filter(|n| !n.is_empty()) {
        for (i, l) in lines.iter().enumerate().skip(start + 1) {
            let indent = l.len() - l.trim_start().len();
            if !l.trim().is_empty() && !l.trim_start().starts_with('#') && indent <= 2 {
                break;
            }
            if l.contains(n) {
                return Some(i + 1);
            }
        }
    }
    Some(start + 1)
}

fn at(line: Option<usize>, msg: impl std::fmt::Display) -> String {
    match line {
        Some(n) => format!("{FILE} line {n}: {msg}"),
        None => format!("{FILE}: {msg}"),
    }
}

/// The first `backticked` word of a serde message: the field or value it is about.
fn quoted(msg: &str) -> Option<&str> {
    msg.split('`').nth(1)
}

/// An https address with a host and no user name or password.
fn https(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| {
        u.scheme() == "https"
            && u.host_str().is_some_and(|h| !h.is_empty())
            && u.username().is_empty()
            && u.password().is_none()
    })
}

/// Scopes that only read: the sign-in scopes, and read or search on a patient's resource type.
fn read_only(scope: &str) -> bool {
    if matches!(scope, "openid" | "fhirUser" | "launch/patient" | "offline_access") {
        return true;
    }
    let Some((ty, access)) = scope.strip_prefix("patient/").and_then(|s| s.split_once('.')) else {
        return false;
    };
    !ty.is_empty() && ty.chars().all(|c| c.is_ascii_alphanumeric()) && matches!(access, "rs" | "read")
}

/// The scopes the launch asks a server of this kind for: SMART v2 read and search, or (a SMART 1.0
/// server) the same types as read; Blue Button's three resources.
fn launch_scopes(kind: &str) -> Vec<Vec<String>> {
    let words = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
    match kind {
        "smart" => vec![words(SMART_SCOPES), words(&oauth::v1_scopes(SMART_SCOPES))],
        "bluebutton" => vec![words(BLUEBUTTON_SCOPES)],
        _ => Vec::new(),
    }
}

/// What is wrong with one vendor: a word of the line it is on, and the message.
fn check_vendor(v: &Vendor) -> Result<(), (&'static str, String)> {
    let key_ok = !v.key.is_empty()
        && v.key.len() <= 40
        && v.key
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !key_ok {
        return Err(("", format!("{:?} is not a vendor key (1 to 40 of a-z, 0-9, _)", v.key)));
    }
    let name = v.name.trim();
    if name.is_empty() || name.chars().count() > 60 || name != v.name {
        return Err(("name:", "name must have 1 to 60 characters and no outer spaces".into()));
    }
    if oauth::kind_spec(&v.kind).is_none() {
        return Err((
            "kind:",
            format!("{:?} is not a connection kind (smart, bluebutton)", v.kind),
        ));
    }
    if !oauth::client_fits(&v.kind, &v.key) {
        return Err((
            "kind:",
            format!("a {} connection cannot use a {:?} client", v.kind, v.key),
        ));
    }
    if v.kind == "bluebutton" && v.client_type != ClientType::Confidential {
        return Err(("client_type:", "Blue Button takes only a confidential client".into()));
    }
    if let Some(bad) = v.servers.iter().find(|s| !https(s)) {
        return Err(("servers:", format!("{bad:?} is not an https address")));
    }
    if v.servers.is_empty() && !v.directory {
        return Err(("directory:", "name the servers, or set directory: true".into()));
    }
    if !https(&v.registration) {
        return Err(("registration:", "registration must be an https address".into()));
    }
    if let Some(bad) = v.sources.iter().find(|s| !https(s)) {
        return Err(("sources:", format!("{bad:?} is not an https address")));
    }
    if let Some(bad) = v.scopes.iter().find(|s| !read_only(s)) {
        return Err(("scopes:", format!("{bad:?} is not a read-only scope")));
    }
    if !launch_scopes(&v.kind).contains(&v.scopes) {
        return Err((
            "scopes:",
            "the scopes must be exactly the ones the launch asks for, in the same order".into(),
        ));
    }
    if v.refresh.unused_expiry_days.is_some_and(|d| !(1..=3650).contains(&d)) {
        return Err(("unused_expiry_days", "unused_expiry_days must be 1 to 3650".into()));
    }
    if v.access_token_seconds.is_some_and(|s| !(60..=86_400).contains(&s)) {
        return Err((
            "access_token_seconds:",
            "access_token_seconds must be 60 to 86400".into(),
        ));
    }
    let documented = v.refresh.rotation != Rotation::Unknown
        || v.refresh.unused_expiry_days.is_some()
        || v.access_token_seconds.is_some();
    if documented && v.sources.is_empty() {
        return Err(("sources:", "a vendor with token values names its sources".into()));
    }
    Ok(())
}

/// Parse and check a profiles file. An error names the file and the line.
pub fn parse(text: &str) -> Result<Profiles, String> {
    let raw: Raw = serde_yaml::from_str(text).map_err(|e| at(e.location().map(|l| l.line()), e))?;
    if raw.version != 1 {
        return Err(at(None, format!("version {} is not 1", raw.version)));
    }
    if raw.redirect_path != CALLBACK_PATH {
        return Err(at(None, format!("redirect_path must be {CALLBACK_PATH}")));
    }
    let mut vendors = Vec::with_capacity(raw.vendors.len());
    for (k, v) in raw.vendors {
        let Some(key) = k.as_str().map(String::from) else {
            return Err(at(None, "every vendor key is a word"));
        };
        let mut vendor: Vendor = serde_yaml::from_value(v).map_err(|e| {
            let msg = e.to_string();
            at(line_of(text, &key, quoted(&msg)), format!("{key}: {msg}"))
        })?;
        vendor.key = key.clone();
        check_vendor(&vendor).map_err(|(word, msg)| at(line_of(text, &key, Some(word)), format!("{key}: {msg}")))?;
        vendors.push(vendor);
    }
    if vendors.is_empty() {
        return Err(at(None, "the file names no vendor"));
    }
    Ok(Profiles {
        version: raw.version,
        redirect_path: raw.redirect_path,
        vendors,
    })
}

fn shipped() -> &'static Result<Profiles, String> {
    static P: OnceLock<Result<Profiles, String>> = OnceLock::new();
    P.get_or_init(|| parse(SHIPPED))
}

/// The shipped profiles; an error says what is wrong with the file, and where.
pub fn profiles() -> Result<&'static Profiles, String> {
    shipped().as_ref().map_err(Clone::clone)
}

/// The server's start check: a bad file stops the start.
pub fn check() -> Result<(), String> {
    profiles().map(|_| ())
}

/// The profile of a household client's vendor. None for a vendor the file does not name.
pub fn vendor(key: &str) -> Option<&'static Vendor> {
    shipped().as_ref().ok()?.get(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_file_parses_and_no_vendor_claims_a_live_check() {
        let p = parse(SHIPPED).expect("connectors.yaml");
        let keys: Vec<&str> = p.vendors.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(
            keys,
            [
                "epic",
                "oracle",
                "athena",
                "healow",
                "nextgen",
                "meditech",
                "va",
                "bluebutton"
            ]
        );
        assert!(p.vendors.iter().all(|v| !v.verified_live));
        assert!(check().is_ok());
        assert_eq!(
            p.redirect_uri("https://ownhealth.rivera.example/"),
            "https://ownhealth.rivera.example/api/connectors/callback"
        );
    }

    #[test]
    fn the_quirks_the_plan_names_are_in_the_file() {
        let v = |k: &str| vendor(k).unwrap_or_else(|| panic!("{k}"));
        assert_eq!(v("oracle").refresh.rotation, Rotation::None);
        assert_eq!(v("oracle").client_type, ClientType::Public);
        assert_eq!(v("oracle").access_token_seconds, Some(570));
        assert_eq!(v("healow").refresh.rotation, Rotation::Rotating);
        assert_eq!(v("healow").client_type, ClientType::Confidential);
        assert_eq!(v("va").refresh.unused_expiry_days, Some(42));
        assert_eq!(v("bluebutton").refresh.rotation, Rotation::SingleUse);
        assert_eq!(v("bluebutton").kind, "bluebutton");
        // SMART 1.0 at the VA: read; everyone else: read and search
        assert!(v("va").scopes.iter().any(|s| s == "patient/Observation.read"));
        assert!(v("epic").scopes.iter().any(|s| s == "patient/Observation.rs"));
        assert!(vendor("labnorte-ehr").is_none());
    }

    /// The shipped file with one line replaced.
    fn edit(from: &str, to: &str) -> String {
        assert!(SHIPPED.contains(from), "{from}");
        SHIPPED.replacen(from, to, 1)
    }

    fn line_holding(text: &str, needle: &str) -> usize {
        text.lines().position(|l| l.contains(needle)).expect(needle) + 1
    }

    #[test]
    fn a_bad_file_names_its_line() {
        // a value of the wrong kind, an unknown field, a bad URL, a write scope, a wrong number
        let cases = [
            (
                "refresh: {rotation: none, unused_expiry_days: 90}",
                "refresh: {rotation: sometimes, unused_expiry_days: 90}",
                "sometimes",
            ),
            (
                "access_token_seconds: 570",
                "access_token_seconds: 570\n    colour: red",
                "colour",
            ),
            (
                "registration: https://code-console.cerner.com/",
                "registration: http://code-console.cerner.com/",
                "http://code-console",
            ),
            (
                "servers: [https://api.va.gov/services/fhir/v0/r4]",
                "servers: [ftp://api.va.gov/fhir]",
                "ftp://",
            ),
            (
                "refresh: {rotation: unknown, unused_expiry_days: 42}",
                "refresh: {rotation: unknown, unused_expiry_days: 0}",
                "unused_expiry_days: 0",
            ),
            ("    kind: bluebutton\n", "    kind: dexcom\n", "kind: dexcom"),
        ];
        for (from, to, needle) in cases {
            let text = edit(from, to);
            let err = parse(&text).expect_err(to);
            let want = format!("{FILE} line {}:", line_holding(&text, needle));
            assert!(err.starts_with(&want), "{to}: {err} (want {want})");
        }
        // a write scope among the scopes
        let text = edit("patient/Observation.rs,", "patient/Observation.cud,");
        let err = parse(&text).expect_err("a write scope");
        assert!(err.contains("not a read-only scope"), "{err}");
        assert!(
            err.starts_with(&format!("{FILE} line {}:", line_holding(&text, "Observation.cud"))),
            "{err}"
        );
        // a broken document: serde_yaml's own line
        let text = SHIPPED.replacen("vendors:\n", "vendors:\n  - [\n", 1);
        let err = parse(&text).expect_err("broken yaml");
        assert!(err.starts_with(&format!("{FILE} line ")), "{err}");
    }

    #[test]
    fn the_scopes_must_be_the_ones_the_launch_asks_for() {
        // one type missing: the person would register less than the launch asks
        let text = edit(
            " patient/Procedure.rs]\n    refresh: {rotation: unknown, unused_expiry_days: null}\n    access_token_seconds: null\n    verified_live: false\n  oracle:",
            " patient/Observation.rs]\n    refresh: {rotation: unknown, unused_expiry_days: null}\n    access_token_seconds: null\n    verified_live: false\n  oracle:",
        );
        let err = parse(&text).expect_err("a scope left out");
        assert!(
            err.contains("epic: ") && err.contains("exactly the ones the launch asks for"),
            "{err}"
        );
    }

    #[test]
    fn token_values_need_a_source_and_blue_button_a_secret() {
        let text = edit(
            "    sources:\n      - https://developer.va.gov/explore/api/patient-health/authorization-code\n",
            "",
        );
        let err = parse(&text).expect_err("no source");
        assert!(
            err.contains("va: a vendor with token values names its sources"),
            "{err}"
        );
        let text = edit(
            "    kind: bluebutton\n    client_type: confidential",
            "    kind: bluebutton\n    client_type: public",
        );
        assert!(parse(&text).expect_err("public bb").contains("confidential"));
    }
}
