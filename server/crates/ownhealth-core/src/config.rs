//! Settings from `OWNHEALTH_*` environment variables, as `ownhealth/config.py` reads them.
//!
//! Every variable has a `*_FILE` twin for secrets mounted as files (Docker secrets):
//! `OWNHEALTH_SECRET_KEY_FILE=/run/secrets/secret_key` wins over `OWNHEALTH_SECRET_KEY`.

use std::fmt;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// The job.queue check constraint in 0001_foundation.sql.
pub const QUEUES: [&str; 2] = ["core", "ocr"];

/// Public: `create_app` refuses it outside dev_auth and demo.
pub const DEV_SECRET_KEY: &str = "dev-insecure-secret-change-me";

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
    pub auto_accept_threshold: f64,
    pub age_of_majority: i32,
    pub host: String,
    pub port: u16,
    /// `OWNHEALTH_SECRETS_KEY`: 32 bytes, standard base64, that seal inbox credentials at rest
    /// (`secrets::SecretBox`). Unset: sources that need a stored secret cannot be saved.
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
    /// `OWNHEALTH_TRUSTED_PROXIES`: comma-separated addresses or CIDR ranges of the proxies in front
    /// of the app. Only a request whose direct peer is one of them has its `X-Forwarded-For` read for
    /// the client address. Empty: the peer address is the client address.
    pub trusted_proxies: Vec<ipnet::IpNet>,
    /// `OWNHEALTH_ANCHOR_DIR`: where each backup writes the audit chain head it anchored, outside the
    /// database (ownhealth-ops backup.rs). Unset: `<data_dir>/anchor`.
    pub anchor_dir: Option<PathBuf>,
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
            auto_accept_threshold: 0.95,
            age_of_majority: 18,
            host: "0.0.0.0".into(),
            port: 8080,
            secrets_key: None,
            ocr: false,
            ocr_models_dir: PathBuf::from("/opt/ownhealth/ocr-models"),
            inbox_dir: None,
            share_url: None,
            trusted_proxies: Vec::new(),
            anchor_dir: None,
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

/// Python's `urllib.parse.quote(s, safe='')`.
fn quote_all(s: &str) -> String {
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
    format!("{scheme}://{user}:{}@{host}", quote_all(password))
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
        let threshold: f64 = parse(env, "AUTO_ACCEPT_THRESHOLD", d.auto_accept_threshold)?;
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
            dev_auth: flag(env, "DEV_AUTH", false)?,
            run_migrations: flag(env, "RUN_MIGRATIONS", true)?,
            run_jobs: flag(env, "RUN_JOBS", true)?,
            job_workers: parse(env, "JOB_WORKERS", d.job_workers)?,
            job_poll_seconds: parse(env, "JOB_POLL_SECONDS", d.job_poll_seconds)?,
            queues: parse_queues(&env_or(env, "QUEUES")?.unwrap_or_else(|| "core".into()))?,
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
            trusted_proxies: parse_trusted_proxies(&env_or(env, "TRUSTED_PROXIES")?.unwrap_or_default())?,
            anchor_dir: env_or(env, "ANCHOR_DIR")?.map(PathBuf::from),
        })
    }

    pub fn blobs_path(&self) -> PathBuf {
        self.blob_dir.clone().unwrap_or_else(|| self.data_dir.join("blobs"))
    }

    /// The folder of the audit anchor file: `OWNHEALTH_ANCHOR_DIR`, or `<data_dir>/anchor`.
    pub fn anchor_path(&self) -> PathBuf {
        self.anchor_dir.clone().unwrap_or_else(|| self.data_dir.join("anchor"))
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
}
