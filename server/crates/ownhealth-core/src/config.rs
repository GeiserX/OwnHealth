//! Settings from `OWNHEALTH_*` environment variables, as `ownhealth/config.py` reads them.
//!
//! Every variable has a `*_FILE` twin for secrets mounted as files (Docker secrets):
//! `OWNHEALTH_SECRET_KEY_FILE=/run/secrets/secret_key` wins over `OWNHEALTH_SECRET_KEY`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
pub use crate::orthanc::OrthancConfig;

/// The job.queue check constraint in 0001_foundation.sql.
pub const QUEUES: [&str; 2] = ["core", "ocr"];

/// Public: `create_app` refuses it outside dev_auth and demo.
pub const DEV_SECRET_KEY: &str = "dev-insecure-secret-change-me";

/// The default of `OWNHEALTH_PARTITION_ROWS`.
pub const DEFAULT_PARTITION_ROWS: u64 = 50_000_000;

/// The values the app runs with. `Debug` leaves out the database URL and the secret key.
#[derive(Clone, PartialEq)]
pub struct Settings {
    pub database_url: String,
    pub data_dir: PathBuf,
    pub blob_dir: Option<PathBuf>,
    /// `OWNHEALTH_STATIC_DIR`: serve the UI from this directory instead of the one built into the binary.
    pub static_dir: Option<PathBuf>,
    pub base_url: String,
    pub default_locale: String,
    pub secret_key: String,
    pub demo: bool,
    /// Unauthenticated requests act as the first owner user. Never on a real install.
    pub dev_auth: bool,
    pub run_migrations: bool,
    pub run_jobs: bool,
    pub job_workers: usize,
    pub job_poll_seconds: f64,
    pub queues: Vec<String>,
    /// `OWNHEALTH_PARTITION_ROWS`: once `wearable_sample` holds more rows than this, the partition job
    /// turns it into monthly partitions (`ownhealth_ops::partition`).
    pub partition_rows: u64,
    pub auto_accept_threshold: f64,
    pub age_of_majority: i32,
    pub host: String,
    pub port: u16,
    /// `OWNHEALTH_SECRETS_KEY`: 32 bytes, standard base64. It seals inbox credentials at rest
    /// (`secrets::SecretBox`) and derives the key that wraps every profile's data key (`keys::Kek`).
    /// Required by every command that reads or writes health data (`require_secrets_key`).
    pub secrets_key: Option<String>,
    /// `OWNHEALTH_OCR`: the `ocr` compose profile is on, so unread pages go to the `ocr` queue instead of
    /// waiting as pending.
    pub ocr: bool,
    /// `OWNHEALTH_OCR_MODELS_DIR`: where `scripts/fetch-ocr-models.sh` put the models.
    pub ocr_models_dir: PathBuf,
    /// `OWNHEALTH_INBOX_DIR`: the watched folder root (`<root>/<profile-slug>/`).
    pub inbox_dir: Option<PathBuf>,
    /// `OWNHEALTH_SHARE_URL`: the public origin of the link-only site block (deploy/Caddyfile), which
    /// every SMART Health Link points at. Unset: sharing is off. https, or http on a loopback host
    /// ([`check_share_url`]).
    pub share_url: Option<String>,
    /// `OWNHEALTH_OW_URL`: the private Open Wearables origin. Unset: the OAuth proxy is off. Both
    /// Compose files set it, so the proxy is on whenever the sidecar can run.
    pub ow_url: Option<reqwest::Url>,
    /// `OWNHEALTH_OW_PUBLIC_URL`: the origin the vendor sends the browser to, before the /ow prefix.
    /// In Compose, the same fallback as Open Wearables' API_BASE_URL. Unset: the guide uses `base_url`.
    pub ow_public_url: Option<String>,
    /// A callback origin Open Wearables would write differently from the setup guide, while the start
    /// cannot tell whether wearables are on (Compose without `wearables` in COMPOSE_PROFILES). The
    /// guide then names the setting to correct instead of a redirect URI.
    pub ow_origin_fix: Option<OwOriginFix>,
    /// `OWNHEALTH_TRUSTED_PROXIES`: comma-separated addresses or CIDR ranges of the proxies in front
    /// of the app. Only a request whose direct peer is one of them has its `X-Forwarded-For` read for
    /// the client address. Empty: the peer address is the client address.
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// `OWNHEALTH_ANCHOR_DIR`: where each backup writes the audit chain head it anchored, outside the
    /// database (ownhealth-ops backup.rs). Unset: `<data_dir>/anchor`.
    pub anchor_dir: Option<PathBuf>,
    /// Orthanc, when the `imaging` compose profile is on ([`OrthancConfig::from_env`]). Every command
    /// reads it, so the viewer proxy of serve and the DICOM import of every worker use the same Orthanc.
    pub orthanc: Option<OrthancConfig>,
    /// `OWNHEALTH_KEYRING_DIR`: the folder of `keyring.json`, the profile keys kept out of the database
    /// dump (`keyfile.rs`). It must sit outside the blob folder and every backup. Unset:
    /// `<data_dir>/keyring`.
    pub keyring_dir: Option<PathBuf>,
    /// `OWNHEALTH_DEV_SHC_ISSUERS`: `https://issuer.example=http://127.0.0.1:9177,...`, test origins of
    /// SMART Health Card issuers and SMART Health Link servers mapped to local servers. Only a debug
    /// build that runs with dev auth reads it (here, and again in routes/shc.rs `issuer_route`); any
    /// other start leaves it unset and never reads the variable or its `_FILE` twin.
    pub dev_shc_issuers: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            database_url: "postgresql://ownhealth:ownhealth@127.0.0.1:5432/ownhealth".into(),
            data_dir: PathBuf::from("./data"),
            blob_dir: None,
            static_dir: None,
            base_url: "http://127.0.0.1:8080".into(),
            default_locale: "es".into(),
            secret_key: DEV_SECRET_KEY.into(),
            demo: false,
            dev_auth: false,
            run_migrations: true,
            run_jobs: true,
            job_workers: 2,
            job_poll_seconds: 2.0,
            queues: vec!["core".into()],
            partition_rows: DEFAULT_PARTITION_ROWS,
            auto_accept_threshold: 0.95,
            age_of_majority: 18,
            host: "0.0.0.0".into(),
            port: 8080,
            secrets_key: None,
            ocr: false,
            ocr_models_dir: PathBuf::from("/opt/ownhealth/ocr-models"),
            inbox_dir: None,
            share_url: None,
            ow_url: None,
            ow_public_url: None,
            ow_origin_fix: None,
            trusted_proxies: Vec::new(),
            anchor_dir: None,
            orthanc: None,
            keyring_dir: None,
            dev_shc_issuers: None,
        }
    }
}

impl fmt::Debug for Settings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Settings")
            .field("data_dir", &self.data_dir)
            .field("blob_dir", &self.blob_dir)
            .field("static_dir", &self.static_dir)
            .field("base_url", &self.base_url)
            .field("default_locale", &self.default_locale)
            .field("demo", &self.demo)
            .field("dev_auth", &self.dev_auth)
            .field("run_migrations", &self.run_migrations)
            .field("run_jobs", &self.run_jobs)
            .field("job_workers", &self.job_workers)
            .field("job_poll_seconds", &self.job_poll_seconds)
            .field("queues", &self.queues)
            .field("partition_rows", &self.partition_rows)
            .field("auto_accept_threshold", &self.auto_accept_threshold)
            .field("age_of_majority", &self.age_of_majority)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("secrets_key", &self.secrets_key.as_ref().map(|_| "***"))
            .field("ocr", &self.ocr)
            .field("ocr_models_dir", &self.ocr_models_dir)
            .field("inbox_dir", &self.inbox_dir)
            .field("share_url", &self.share_url)
            .field("trusted_proxies", &self.trusted_proxies)
            .field("anchor_dir", &self.anchor_dir)
            .field("orthanc", &self.orthanc)
            .field("keyring_dir", &self.keyring_dir)
            .field("dev_shc_issuers", &self.dev_shc_issuers)
            .finish_non_exhaustive()
    }
}

/// Where the environment comes from: the process, or a map in tests.
pub trait Env {
    fn var(&self, name: &str) -> Option<String>;
}

/// The process environment.
pub struct ProcessEnv;

impl Env for ProcessEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

impl<S: std::hash::BuildHasher> Env for std::collections::HashMap<String, String, S> {
    fn var(&self, name: &str) -> Option<String> {
        self.get(name).cloned()
    }
}

/// `OWNHEALTH_<name>_FILE` (its content, trimmed) wins over `OWNHEALTH_<name>`.
fn env_value(env: &dyn Env, name: &str) -> Result<Option<String>> {
    if let Some(path) = env.var(&format!("OWNHEALTH_{name}_FILE")).filter(|p| !p.is_empty()) {
        let text =
            std::fs::read_to_string(&path).map_err(|e| Error::Invalid(format!("OWNHEALTH_{name}_FILE={path}: {e}")))?;
        return Ok(Some(text.trim().to_string()));
    }
    Ok(env.var(&format!("OWNHEALTH_{name}")))
}

/// Python's `_env(name) or default`: an empty value falls through to the default.
fn env_or(env: &dyn Env, name: &str) -> Result<Option<String>> {
    Ok(env_value(env, name)?.filter(|v| !v.is_empty()))
}

fn flag(env: &dyn Env, name: &str, default: bool) -> Result<bool> {
    Ok(match env_value(env, name)? {
        None => default,
        Some(v) => matches!(v.trim().to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
    })
}

fn parse<T: std::str::FromStr>(env: &dyn Env, name: &str, default: T) -> Result<T> {
    match env_or(env, name)? {
        None => Ok(default),
        Some(v) => v
            .trim()
            .parse()
            .map_err(|_| Error::Invalid(format!("OWNHEALTH_{name} is not a number: {v:?}"))),
    }
}

/// Parse `OWNHEALTH_QUEUES`: trim entries, drop empty ones, refuse a name no job row can carry.
pub fn parse_queues(value: &str) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    for q in value.split(',').map(str::trim).filter(|q| !q.is_empty()) {
        if !names.iter().any(|n| n == q) {
            names.push(q.to_string());
        }
    }
    if names.is_empty() || names.iter().any(|q| !QUEUES.contains(&q.as_str())) {
        return Err(Error::Invalid(format!(
            "OWNHEALTH_QUEUES must name some of {}; got {value:?}",
            QUEUES.join(", ")
        )));
    }
    Ok(names)
}

/// `OWNHEALTH_SHARE_URL` as the links carry it: an origin without a trailing slash. A recipient's
/// name and the passcode travel to it, so it is https; plain http only reaches a loopback host
/// (a tunnel or a test on the same machine). A path, query, fragment, user name or password is refused.
/// No refusal prints the value: a user and password typed without `https://` parse as a scheme and a
/// path, so no parse can be trusted to find them first.
pub fn check_share_url(value: &str) -> Result<String> {
    let refuse = |why: &str| {
        Error::Invalid(format!(
            "OWNHEALTH_SHARE_URL {why}. Set it to the public https address of the share port, such as \
             https://share.example.org, or leave it unset to keep sharing off."
        ))
    };
    let origin = value.trim().trim_end_matches('/');
    let url = url::Url::parse(origin).map_err(|_| {
        Error::Invalid(
            "OWNHEALTH_SHARE_URL is not a URL. Set it to the public https address of the share port, \
             such as https://share.example.org, or leave it unset to keep sharing off."
                .into(),
        )
    })?;
    // every link and QR code would repeat it, and a browser refuses such a URL
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::Invalid(
            "OWNHEALTH_SHARE_URL carries a user name or password. Take it out: every link and QR code \
             would repeat it. Set it to the public https address of the share port, such as \
             https://share.example.org, or leave it unset to keep sharing off."
                .into(),
        ));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err(refuse("has no host")),
    };
    match url.scheme() {
        "https" => {}
        "http" if loopback => {}
        "http" => return Err(refuse("uses http: only https may carry a passcode off this machine")),
        _ => return Err(refuse("is not an https address")),
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(refuse("has more than a scheme, a host and a port"));
    }
    // the parsed origin, not the text: `https:share.example.org` parses, and links built on the text
    // would lack the `//` and resolve against the page's own host
    Ok(url.origin().ascii_serialization())
}

/// Parse `OWNHEALTH_TRUSTED_PROXIES`: addresses (`172.18.0.3`) or ranges (`172.16.0.0/12`).
pub fn parse_trusted_proxies(value: &str) -> Result<Vec<ipnet::IpNet>> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| {
            v.parse::<ipnet::IpNet>()
                .or_else(|_| v.parse::<std::net::IpAddr>().map(ipnet::IpNet::from))
                .map_err(|_| {
                    Error::Invalid(format!(
                        "OWNHEALTH_TRUSTED_PROXIES: {v:?} is not an address or a range such as 172.16.0.0/12"
                    ))
                })
        })
        .collect()
}

/// Percent-encode one URL component, leaving only unreserved characters.
pub fn quote_url_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Put `OWNHEALTH_DB_PASSWORD` into a URL that names a user but no password.
fn with_password(url: &str, password: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let Some((user, host)) = rest.split_once('@') else {
        return url.to_string();
    };
    if user.contains(':') {
        return url.to_string();
    }
    format!("{scheme}://{user}:{}@{host}", quote_url_component(password))
}

/// An operator-supplied HTTP origin, with no path, query or credentials.
fn ow_origin(value: &str, setting: &str) -> Result<reqwest::Url> {
    reqwest::Url::parse(value)
        .ok()
        .filter(|u| {
            matches!(u.scheme(), "http" | "https")
                && u.host().is_some()
                && u.username().is_empty()
                && u.password().is_none()
                && u.path() == "/"
                && u.query().is_none()
                && u.fragment().is_none()
        })
        .ok_or_else(|| Error::Invalid(format!("{setting} must be an HTTP or HTTPS origin.")))
}

/// The setting that makes the Open Wearables callback origin, and how to write it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwOriginFix {
    pub setting: &'static str,
    /// The value written as Open Wearables needs it. None: the value is no HTTP or HTTPS origin.
    pub value: Option<String>,
    message: String,
}

impl OwOriginFix {
    /// The start error that names the setting and the value to write.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// The callback origin, or the fix when Open Wearables would write it differently from the guide.
fn configured_ow_public_url(
    env: &dyn Env,
    compose_fallback: Option<String>,
) -> Result<std::result::Result<Option<String>, OwOriginFix>> {
    const PUBLIC: &str = "OWNHEALTH_OW_PUBLIC_URL";
    const DOMAIN: &str = "OWNHEALTH_DOMAIN";
    let public = if compose_fallback.is_some() {
        // The sidecar receives the interpolated string, including when a _FILE twin is set.
        env.var(PUBLIC).filter(|value| !value.is_empty())
    } else {
        env_or(env, "OW_PUBLIC_URL")?
    };
    let (value, source) = match (public, compose_fallback) {
        (Some(value), _) => (value, Some(PUBLIC)),
        (None, Some(value)) => {
            let source = match env.var("OWNHEALTH_OW_COMPOSE_FALLBACK_VAR").as_deref() {
                Some(DOMAIN) => Some(DOMAIN),
                Some("OWNHEALTH_BASE_URL") => Some("OWNHEALTH_BASE_URL"),
                _ => None, // Quickstart derived the origin from its published port.
            };
            (value, source)
        }
        (None, None) => return Ok(Ok(None)),
    };
    let canonical = match ow_origin(&value, source.unwrap_or(PUBLIC)) {
        Ok(url) => url.origin().ascii_serialization(),
        Err(err) => {
            return Ok(Err(OwOriginFix {
                setting: source.unwrap_or(PUBLIC),
                value: None,
                message: err.to_string(),
            }));
        }
    };
    if value != canonical {
        // OW appends its callback path to the string as written. Normalizing only the guide
        // would tell the operator to register a redirect URI different from the one OW sends.
        let (setting, written, guidance) = match source {
            Some(DOMAIN) => {
                let host = canonical.trim_start_matches("https://").to_string();
                let guidance = format!("{DOMAIN} must be written as {host}");
                (DOMAIN, host, guidance)
            }
            Some(setting) => (
                setting,
                canonical.clone(),
                format!("{setting} must be written as {canonical}"),
            ),
            None => (
                PUBLIC,
                canonical.clone(),
                format!("Set {PUBLIC}={canonical} for the callback origin"),
            ),
        };
        return Ok(Err(OwOriginFix {
            setting,
            value: Some(written),
            message: format!("{guidance} because Open Wearables builds its redirect URI from the value as written."),
        }));
    }
    Ok(Ok(Some(value)))
}

impl Settings {
    pub fn from_env() -> Result<Settings> {
        Settings::from(&ProcessEnv)
    }

    /// The same rules as `Settings.from_env` in config.py, over any environment.
    pub fn from(env: &dyn Env) -> Result<Settings> {
        let d = Settings::default();
        let mut url = env_or(env, "DATABASE_URL")?
            .or_else(|| env.var("DATABASE_URL").filter(|v| !v.is_empty()))
            .unwrap_or(d.database_url);
        if let Some(password) = env_or(env, "DB_PASSWORD")? {
            url = with_password(&url, &password);
        }
        // Both Compose files pass the private URL to every install, because `--profile wearables` on
        // the command line starts the sidecar without telling the app. So a callback origin Open
        // Wearables would write differently refuses the start only when wearables are known to be on:
        // named in COMPOSE_PROFILES, or the URL set outside Compose. Otherwise the guide names the fix.
        let ow_compose_fallback = env.var("OWNHEALTH_OW_COMPOSE_FALLBACK").filter(|v| !v.is_empty());
        let ow_known_on = ow_compose_fallback.is_none()
            || env.var("COMPOSE_PROFILES").is_some_and(|profiles| {
                profiles
                    .split(',')
                    .any(|profile| matches!(profile.trim(), "wearables" | "*"))
            });
        let ow_url = env_or(env, "OW_URL")?
            .map(|s| ow_origin(&s, "OWNHEALTH_OW_URL"))
            .transpose()?;
        let (ow_public_url, ow_origin_fix) = match ow_url {
            None => (None, None),
            Some(_) => match configured_ow_public_url(env, ow_compose_fallback)? {
                Ok(origin) => (origin, None),
                Err(fix) if ow_known_on => return Err(Error::Invalid(fix.message)),
                Err(fix) => (None, Some(fix)),
            },
        };
        let threshold: f64 = parse(env, "AUTO_ACCEPT_THRESHOLD", d.auto_accept_threshold)?;
        let dev_auth = flag(env, "DEV_AUTH", false)?;
        // only a debug build with dev auth uses the map (routes/shc.rs), so no other start reads it
        let dev_shc_issuers = if cfg!(debug_assertions) && dev_auth {
            env_or(env, "DEV_SHC_ISSUERS")?
        } else {
            None
        };
        Ok(Settings {
            database_url: url,
            data_dir: env_or(env, "DATA_DIR")?.map(PathBuf::from).unwrap_or(d.data_dir),
            blob_dir: env_or(env, "BLOB_DIR")?.map(PathBuf::from),
            static_dir: env_or(env, "STATIC_DIR")?.map(PathBuf::from),
            base_url: env_or(env, "BASE_URL")?
                .or(env_or(env, "ORIGIN")?)
                .unwrap_or(d.base_url)
                .trim_end_matches('/')
                .to_string(),
            default_locale: env_or(env, "DEFAULT_LOCALE")?.unwrap_or(d.default_locale),
            secret_key: env_or(env, "SECRET_KEY")?.unwrap_or(d.secret_key),
            demo: flag(env, "DEMO", false)?,
            dev_auth,
            run_migrations: flag(env, "RUN_MIGRATIONS", true)?,
            run_jobs: flag(env, "RUN_JOBS", true)?,
            job_workers: parse(env, "JOB_WORKERS", d.job_workers)?,
            job_poll_seconds: parse(env, "JOB_POLL_SECONDS", d.job_poll_seconds)?,
            queues: parse_queues(&env_or(env, "QUEUES")?.unwrap_or_else(|| "core".into()))?,
            partition_rows: parse(env, "PARTITION_ROWS", d.partition_rows)?,
            auto_accept_threshold: threshold.max(0.90),
            age_of_majority: parse(env, "AGE_OF_MAJORITY", d.age_of_majority)?,
            host: env_or(env, "HOST")?.unwrap_or(d.host),
            port: parse(env, "PORT", d.port)?,
            secrets_key: env_or(env, "SECRETS_KEY")?,
            ocr: flag(env, "OCR", false)?,
            ocr_models_dir: env_or(env, "OCR_MODELS_DIR")?
                .map(PathBuf::from)
                .unwrap_or(d.ocr_models_dir),
            inbox_dir: env_or(env, "INBOX_DIR")?.map(PathBuf::from),
            share_url: env_or(env, "SHARE_URL")?.map(|u| check_share_url(&u)).transpose()?,
            ow_url,
            ow_public_url,
            ow_origin_fix,
            trusted_proxies: parse_trusted_proxies(&env_or(env, "TRUSTED_PROXIES")?.unwrap_or_default())?,
            anchor_dir: env_or(env, "ANCHOR_DIR")?.map(PathBuf::from),
            // serve's viewer proxy and every worker's DICOM import read the same Orthanc
            orthanc: OrthancConfig::from_env(env)?,
            keyring_dir: env_or(env, "KEYRING_DIR")?.map(PathBuf::from),
            dev_shc_issuers,
        })
    }

    /// The one-line refusal of a start without a usable OWNHEALTH_SECRETS_KEY: since 1.0 every
    /// profile's files are encrypted under keys it wraps, so nothing may run without it.
    pub fn require_secrets_key(&self) -> Result<()> {
        crate::keys::Kek::from_settings(self)
            .map(|_| ())
            .map_err(|e| Error::Invalid(e.0))
    }

    pub fn blobs_path(&self) -> PathBuf {
        self.blob_dir.clone().unwrap_or_else(|| self.data_dir.join("blobs"))
    }

    /// The folder of the audit anchor file: `OWNHEALTH_ANCHOR_DIR`, or `<data_dir>/anchor`.
    pub fn anchor_path(&self) -> PathBuf {
        self.anchor_dir.clone().unwrap_or_else(|| self.data_dir.join("anchor"))
    }

    /// The folder of `keyring.json`: `OWNHEALTH_KEYRING_DIR`, or `<data_dir>/keyring`.
    pub fn keyring_path(&self) -> PathBuf {
        self.keyring_dir
            .clone()
            .unwrap_or_else(|| self.data_dir.join("keyring"))
    }

    /// The URL for sqlx: `postgresql+psycopg://` (accepted by the Python app) becomes `postgresql://`.
    pub fn sqlx_url(&self) -> String {
        sqlx_url(&self.database_url)
    }

    /// The data directory, for code that writes beside the blobs (exports, backups).
    pub fn data_path(&self) -> &Path {
        &self.data_dir
    }
}

pub fn sqlx_url(url: &str) -> String {
    for prefix in ["postgresql+psycopg://", "postgres+psycopg://"] {
        if let Some(rest) = url.strip_prefix(prefix) {
            return format!("postgresql://{rest}");
        }
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn db_password_is_url_quoted() {
        let e = env(&[
            ("OWNHEALTH_DATABASE_URL", "postgresql://ownhealth@db:5432/ownhealth"),
            ("OWNHEALTH_DB_PASSWORD", "a/b+c=d@e:f"),
        ]);
        let s = Settings::from(&e).expect("settings");
        assert_eq!(
            s.database_url,
            "postgresql://ownhealth:a%2Fb%2Bc%3Dd%40e%3Af@db:5432/ownhealth"
        );
    }

    #[test]
    fn every_command_reads_orthanc_from_the_environment() {
        // the worker runs the DICOM import too, so Orthanc is in the settings every command reads
        let e = env(&[
            ("OWNHEALTH_ORTHANC_URL", "http://orthanc:8042/"),
            ("OWNHEALTH_ORTHANC_PASSWORD", "synthetic-orthanc-password"),
        ]);
        let orthanc = Settings::from(&e).expect("settings").orthanc.expect("imaging on");
        assert_eq!(orthanc.url, "http://orthanc:8042");
        assert_eq!(orthanc.user, "ownhealth");
        assert!(Settings::from(&env(&[])).expect("settings").orthanc.is_none());
        // a URL without a password stops every command, as it stops serve
        assert!(Settings::from(&env(&[("OWNHEALTH_ORTHANC_URL", "http://orthanc:8042")])).is_err());
    }

    /// The dev issuer map is read only where the route can use it: a debug build with dev auth. Any
    /// other start never reads it, so a missing map file stops nothing.
    #[test]
    fn the_dev_issuer_map_is_read_only_by_a_debug_build_with_dev_auth() {
        let missing = "/nonexistent/ownhealth-dev-shc-issuers";
        let e = env(&[("OWNHEALTH_DEV_SHC_ISSUERS_FILE", missing)]);
        let s = Settings::from(&e).expect("the map file was read without dev auth");
        assert_eq!(s.dev_shc_issuers, None);
        let map = "https://issuer.labnorte.example=http://127.0.0.1:9177";
        let e = env(&[("OWNHEALTH_DEV_SHC_ISSUERS", map)]);
        assert_eq!(Settings::from(&e).expect("settings").dev_shc_issuers, None);
        let e = env(&[("OWNHEALTH_DEV_AUTH", "1"), ("OWNHEALTH_DEV_SHC_ISSUERS", map)]);
        let got = Settings::from(&e).expect("settings").dev_shc_issuers;
        // a release build (`cargo test --release`) never reads it
        let want = cfg!(debug_assertions).then(|| map.to_string());
        assert_eq!(got, want);
    }

    #[test]
    fn file_twin_wins() {
        let dir = tempfile::tempdir().expect("tmp");
        let secret = dir.path().join("secret");
        std::fs::write(&secret, "from-file\n").expect("write");
        let path = secret.to_string_lossy().to_string();
        let e = env(&[
            ("OWNHEALTH_SECRET_KEY", "from-env"),
            ("OWNHEALTH_SECRET_KEY_FILE", &path),
        ]);
        assert_eq!(Settings::from(&e).expect("settings").secret_key, "from-file");
    }

    #[test]
    fn queues_are_trimmed_and_unknown_names_refused() {
        let e = env(&[("OWNHEALTH_QUEUES", "core, ocr,")]);
        assert_eq!(Settings::from(&e).expect("settings").queues, vec!["core", "ocr"]);
        let e = env(&[("OWNHEALTH_QUEUES", "core,ocrr")]);
        let err = Settings::from(&e).expect_err("refused");
        assert!(err.to_string().contains("OWNHEALTH_QUEUES"));
    }

    #[test]
    fn the_secrets_key_is_required_with_a_one_line_fix() {
        let err = Settings::default().require_secrets_key().expect_err("refused");
        let text = err.to_string();
        assert!(text.starts_with("OWNHEALTH_SECRETS_KEY is not set"), "{text}");
        assert!(
            text.contains("openssl rand -base64 32") && !text.contains('\n'),
            "{text}"
        );
        let bad = Settings {
            secrets_key: Some("not-base64!".into()),
            ..Settings::default()
        };
        assert!(
            bad.require_secrets_key()
                .expect_err("refused")
                .to_string()
                .contains("base64")
        );
        let mut secret = [0u8; 32];
        rand::fill(&mut secret[..]);
        let good = Settings {
            secrets_key: Some(base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                secret,
            )),
            ..Settings::default()
        };
        assert!(good.require_secrets_key().is_ok());
    }

    #[test]
    fn debug_hides_the_database_password_and_the_secret_key() {
        let s = Settings {
            database_url: "postgresql://oh:pw-x1@db/oh".into(),
            secret_key: "sk-x2".into(),
            secrets_key: Some("enc-x3".into()),
            ..Settings::default()
        };
        let shown = format!("{s:?}");
        assert!(!shown.contains("pw-x1") && !shown.contains("sk-x2") && !shown.contains("enc-x3"));
    }

    #[test]
    fn flags_threshold_and_base_url() {
        let e = env(&[
            ("OWNHEALTH_DEMO", " Yes "),
            ("OWNHEALTH_RUN_JOBS", ""),
            ("OWNHEALTH_AUTO_ACCEPT_THRESHOLD", "0.5"),
            ("OWNHEALTH_ORIGIN", "https://health.example/"),
        ]);
        let s = Settings::from(&e).expect("settings");
        assert!(s.demo && !s.run_jobs && s.run_migrations);
        assert_eq!(s.auto_accept_threshold, 0.90);
        assert_eq!(s.base_url, "https://health.example");
        assert_eq!(s.blobs_path(), PathBuf::from("./data/blobs"));
        assert_eq!(sqlx_url("postgresql+psycopg://u@h/d"), "postgresql://u@h/d");
    }

    /// The share address carries a recipient's name and a passcode: https, or http on loopback only.
    #[test]
    fn the_share_url_is_https_or_loopback_http_and_only_an_origin() {
        let share = |v: &str| Settings::from(&env(&[("OWNHEALTH_SHARE_URL", v)])).map(|s| s.share_url);
        for (given, kept) in [
            ("https://share.example.org/", "https://share.example.org"),
            ("https://share.example.org:8443", "https://share.example.org:8443"),
            ("http://127.0.0.1:18090", "http://127.0.0.1:18090"),
            ("http://localhost:8090/", "http://localhost:8090"),
            ("http://[::1]:8090", "http://[::1]:8090"),
            // parses without the `//`: the links carry the parsed origin, never the text
            ("https:share.example.org", "https://share.example.org"),
        ] {
            assert_eq!(share(given).ok().flatten().as_deref(), Some(kept), "{given}");
        }
        for bad in [
            "http://example.org",
            "http://192.168.1.20:8090",
            "ftp://share.example.org",
            "share.example.org",
            "https://share.example.org/links",
            "https://share.example.org/?x=1",
            "https://user@share.example.org",
        ] {
            let err = share(bad).expect_err(bad).to_string();
            assert!(
                err.contains("OWNHEALTH_SHARE_URL") && err.contains("https"),
                "{bad}: {err}"
            );
        }
        // a user or a password would be copied into every link and QR code: refused, and said plainly
        for creds in [
            "https://:hunter2@share.example.org",
            "https://user:hunter2@share.example.org",
            "https://user@share.example.org",
        ] {
            let err = share(creds).expect_err(creds).to_string();
            assert!(
                err.contains("user name or password") && !err.contains("hunter2"),
                "{creds}: {err}"
            );
        }
        // a value that does not parse (a port that is not a number) is refused without being printed
        let err = share("https://user:hunter2@share.example.org:port")
            .expect_err("bad port")
            .to_string();
        assert!(
            err.contains("OWNHEALTH_SHARE_URL is not a URL") && !err.contains("hunter2"),
            "{err}"
        );
        // without `https://` the user and password parse as a scheme and a path, and still never print
        for typo in [
            "user:hunter2@share.example.org",
            "share:hunter2@share.example.org:443",
            "https://user:hunter2@",
        ] {
            let err = share(typo).expect_err(typo).to_string();
            assert!(
                err.contains("OWNHEALTH_SHARE_URL") && !err.contains("hunter2"),
                "{typo}: {err}"
            );
        }
        assert_eq!(share("").ok().flatten(), None, "empty keeps sharing off");
    }

    #[test]
    fn trusted_proxies_are_addresses_or_ranges() {
        let e = env(&[("OWNHEALTH_TRUSTED_PROXIES", " 172.16.0.0/12, 10.0.0.7 ,fc00::/7")]);
        let got: Vec<String> = Settings::from(&e)
            .expect("settings")
            .trusted_proxies
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(got, ["172.16.0.0/12", "10.0.0.7/32", "fc00::/7"]);
        assert!(Settings::from(&env(&[])).expect("settings").trusted_proxies.is_empty());
        let err = Settings::from(&env(&[("OWNHEALTH_TRUSTED_PROXIES", "caddy")])).expect_err("refused");
        assert!(err.to_string().contains("OWNHEALTH_TRUSTED_PROXIES"));
    }

    #[test]
    fn open_wearables_public_url_must_be_written_as_its_canonical_origin() {
        // Compose hands Open Wearables the value as written and it builds its redirect URI from that,
        // so any value the guide would show differently is refused, naming the form to write.
        let cases = [
            ("https://CB.example.test", "https://cb.example.test"),
            ("https://cb.example.test:443", "https://cb.example.test"),
            ("http://localhost:80", "http://localhost"),
            ("https://cb.example.test ", "https://cb.example.test"),
            ("https://cb.example.test/", "https://cb.example.test"),
            ("https://cb.example.test/ ", "https://cb.example.test"),
            ("https://bücher.example.test", "https://xn--bcher-kva.example.test"),
        ];
        for (value, canonical) in cases {
            let err = Settings::from(&env(&[
                ("OWNHEALTH_OW_URL", "http://ow-app:8000"),
                ("OWNHEALTH_OW_PUBLIC_URL", value),
            ]))
            .expect_err(value)
            .to_string();
            assert!(
                err.contains(&format!("OWNHEALTH_OW_PUBLIC_URL must be written as {canonical}")),
                "{value}: {err}"
            );
        }
        for value in [
            "https://cb.example.test",
            "https://cb.example.test:8443",
            "http://localhost:8080",
            "http://[::1]:8080",
        ] {
            let s = Settings::from(&env(&[
                ("OWNHEALTH_OW_URL", "http://ow-app:8000"),
                ("OWNHEALTH_OW_PUBLIC_URL", value),
            ]))
            .expect(value);
            assert_eq!(s.ow_public_url.as_deref(), Some(value));
        }
    }

    #[test]
    fn a_compose_start_without_the_wearables_profile_names_the_origin_fix_instead_of_failing() {
        let compose = |profiles: &str, public: &str| {
            env(&[
                ("OWNHEALTH_OW_URL", "http://ow-app:8000"),
                ("OWNHEALTH_OW_PUBLIC_URL", public),
                ("OWNHEALTH_OW_COMPOSE_FALLBACK", "https://Health.Example.test:443"),
                ("OWNHEALTH_OW_COMPOSE_FALLBACK_VAR", "OWNHEALTH_DOMAIN"),
                ("COMPOSE_PROFILES", profiles),
            ])
        };
        for (public, setting, value) in [
            ("", "OWNHEALTH_DOMAIN", Some("health.example.test")),
            (
                "https://cb.example.test/",
                "OWNHEALTH_OW_PUBLIC_URL",
                Some("https://cb.example.test"),
            ),
            ("cb.example.test", "OWNHEALTH_OW_PUBLIC_URL", None),
        ] {
            let s = Settings::from(&compose("ocr", public)).expect(public);
            assert!(s.ow_url.is_some() && s.ow_public_url.is_none(), "{public}");
            let fix = s.ow_origin_fix.expect(public);
            assert_eq!((fix.setting, fix.value.as_deref()), (setting, value), "{public}");
            // With the profile named, the start refuses with the same words.
            for profiles in ["wearables", "ocr, wearables", "*"] {
                let err = Settings::from(&compose(profiles, public))
                    .expect_err(profiles)
                    .to_string();
                assert!(err.contains(fix.message()), "{profiles}: {err}");
            }
        }
        let s = Settings::from(&compose("", "https://cb.example.test")).expect("canonical");
        assert_eq!(s.ow_public_url.as_deref(), Some("https://cb.example.test"));
        assert!(s.ow_origin_fix.is_none());
    }

    #[test]
    fn open_wearables_urls_are_bare_origins() {
        let s = Settings::from(&env(&[
            ("OWNHEALTH_OW_URL", "http://ow-app:8000"),
            ("OWNHEALTH_OW_PUBLIC_URL", "https://cb.example.test"),
        ]))
        .expect("settings");
        assert_eq!(s.ow_url.map(String::from).as_deref(), Some("http://ow-app:8000/"));
        assert_eq!(s.ow_public_url.as_deref(), Some("https://cb.example.test"));
        let bad = [
            "http://ow-app:8000/api",
            "http://user:secret@ow-app:8000",
            "http://ow-app:8000?x=1",
            "http://ow-app:8000#top",
            "ftp://ow-app:8000",
            "ow-app:8000",
        ];
        for setting in ["OWNHEALTH_OW_URL", "OWNHEALTH_OW_PUBLIC_URL"] {
            for value in bad {
                let mut vars = env(&[("OWNHEALTH_OW_URL", "http://ow-app:8000")]);
                vars.insert(setting.into(), value.into());
                let err = Settings::from(&vars).expect_err(value).to_string();
                assert!(
                    err.contains(setting) && !err.contains("secret"),
                    "{setting}={value}: {err}"
                );
            }
        }
        // Open Wearables appends /ow to the raw value, so a trailing slash would give it //ow while
        // the guide shows /ow. Compose falls back to OWNHEALTH_BASE_URL, so this catches that too.
        for value in ["https://cb.example.test/", "http://localhost:8080/"] {
            let err = Settings::from(&env(&[
                ("OWNHEALTH_OW_URL", "http://ow-app:8000"),
                ("OWNHEALTH_OW_PUBLIC_URL", value),
            ]))
            .expect_err(value)
            .to_string();
            let canonical = value.trim_end_matches('/');
            assert!(
                err.contains(&format!("must be written as {canonical}")),
                "{value}: {err}"
            );
        }
    }
}
