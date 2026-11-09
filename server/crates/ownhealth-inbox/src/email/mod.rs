//! Lane A: email-in (ingestion.md 3.2, security-privacy.md 4.2 #7, SPIKES-v0.2.md b).
//!
//! A dedicated mailbox, polled every `source.poll_seconds` (default 300) over IMAP (async-imap on
//! tokio, rustls). Each new message (by Message-ID, `dedup::seen_item`) is stored whole as a blob
//! first. Then [`route_message`] decides: an allowlisted sender whose domain passed DKIM or SPF in an
//! `Authentication-Results` header stamped by the trusted authserv-id (or a rule with
//! `trust_unauthenticated`) goes to that rule's profile; everything else is held in quarantine
//! (`core::quarantine::hold`), never parsed further. Attachments of allowed mail go through
//! `core::arrive::arrive` (a locked PDF is unlocked first with that sender's passwords, the copy
//! stored with `derived_from`); the text body is stored as its own document. Processed mail moves to
//! the done folder (`OwnHealth/Done`), flagged `\Seen`.
//!
//! Source `config` (kind `email`): `{host, port, tls: "implicit"|"starttls"|"none", username,
//! folder?: "INBOX", done_folder?: "OwnHealth/Done", trusted_authserv_id?: string}`; the password is
//! the source's secret (purpose `imap_password`).

pub mod pdf_password;

use ownhealth_core::blobs::BlobStore;
use ownhealth_core::error::{Error, Result};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::secrets::{SecretBox, SecretString};
use ownhealth_core::sources::{RunOutcome, Source};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

/// Limits of ingestion.md 3.2.
pub const MAX_ATTACHMENT_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_ATTACHMENTS: usize = 50;
pub const DEFAULT_POLL_SECONDS: i32 = 300;
pub const DEFAULT_DONE_FOLDER: &str = "OwnHealth/Done";

/// `source.config` of an email source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MailboxConfig {
    pub host: String,
    pub port: u16,
    /// implicit | starttls | none
    pub tls: String,
    pub username: String,
    #[serde(default = "inbox")]
    pub folder: String,
    #[serde(default = "done_folder")]
    pub done_folder: String,
    /// The authserv-id of the receiving server (RFC 8601); other Authentication-Results are ignored.
    #[serde(default)]
    pub trusted_authserv_id: Option<String>,
}

fn inbox() -> String {
    "INBOX".into()
}

fn done_folder() -> String {
    DEFAULT_DONE_FOLDER.into()
}

/// One allowlist row (`sender_rule`).
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
pub struct SenderRule {
    pub id: Uuid,
    pub source_id: Uuid,
    /// `resultados@labnorte.example` or the domain `labnorte.example`, lower case.
    pub pattern: String,
    pub profile_id: Uuid,
    pub doc_type_hint: Option<String>,
    pub trust_unauthenticated: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SenderRuleIn {
    pub pattern: String,
    pub profile_id: Uuid,
    #[serde(default)]
    pub doc_type_hint: Option<String>,
    #[serde(default)]
    pub trust_unauthenticated: bool,
}

/// What routing needs from a message, read from its headers only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageFacts {
    /// The From address, lower case.
    pub from: Option<String>,
    pub message_id: Option<String>,
    /// Untrusted text.
    pub subject: Option<String>,
    /// Every `Authentication-Results` header value, verbatim.
    pub authentication_results: Vec<String>,
    /// `(filename, mime, size)`; names are untrusted text.
    pub attachments: Vec<(Option<String>, String, usize)>,
}

/// Where a message goes.
#[derive(Debug, Clone, PartialEq)]
pub enum Routing {
    Allowed {
        rule: SenderRule,
    },
    /// A `core::quarantine::REASONS` code.
    Quarantine {
        reason: &'static str,
    },
}

/// What one message became.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MessageOutcome {
    pub quarantine_id: Option<Uuid>,
    pub document_ids: Vec<Uuid>,
    pub duplicates: i32,
    pub skipped: Vec<String>,
}

/// Read the routing facts from a raw message (RFC 5322 bytes).
pub fn message_facts(_raw: &[u8]) -> Result<MessageFacts> {
    Err(Error::NotImplemented("email::message_facts"))
}

/// The allowlist decision (pure). Sender match: an address rule beats a domain rule.
pub fn route_message(_facts: &MessageFacts, _rules: &[SenderRule], _trusted_authserv_id: Option<&str>) -> Routing {
    Routing::Quarantine {
        reason: "sender_not_allowed",
    }
}

/// One raw message from a mailbox: store, route, quarantine or arrive. `released` skips the routing
/// for a message a person released (it must still map to a profile).
pub async fn ingest_message(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _sb: Option<&SecretBox>,
    _source: &Source,
    _raw: &[u8],
    _released_to: Option<Uuid>,
) -> Result<MessageOutcome> {
    Err(Error::NotImplemented("email::ingest_message"))
}

/// The `email` poller: log in, fetch unseen mail, ingest each message, move it to the done folder.
pub async fn poll_mailbox(_ctx: JobContext, _source: Source) -> std::result::Result<RunOutcome, JobError> {
    Err(Error::NotImplemented("email::poll_mailbox").into())
}

/// Release a held message into `profile_id`; with `allow_sender`, also add its sender to the
/// allowlist. Returns the documents it produced. Needs a person (the route checks step-up policy).
pub async fn release(
    _conn: &mut PgConnection,
    _blobs: &BlobStore,
    _sb: Option<&SecretBox>,
    _quarantine_id: Uuid,
    _user_id: Uuid,
    _profile_id: Uuid,
    _allow_sender: bool,
) -> Result<Vec<Uuid>> {
    Err(Error::NotImplemented("email::release"))
}

pub async fn list_rules(_conn: &mut PgConnection, _source_id: Uuid) -> Result<Vec<SenderRule>> {
    Err(Error::NotImplemented("email::list_rules"))
}

pub async fn add_rule(
    _conn: &mut PgConnection,
    _source_id: Uuid,
    _rule: &SenderRuleIn,
    _user_id: Uuid,
) -> Result<Uuid> {
    Err(Error::NotImplemented("email::add_rule"))
}

pub async fn delete_rule(_conn: &mut PgConnection, _rule_id: Uuid) -> Result<bool> {
    Err(Error::NotImplemented("email::delete_rule"))
}

/// Store a PDF password for one sender rule (sealed, purpose `pdf_password`). Returns the rule row id.
pub async fn add_pdf_password(
    _conn: &mut PgConnection,
    _sb: &SecretBox,
    _sender_rule_id: Uuid,
    _password: &SecretString,
    _label: Option<&str>,
) -> Result<Uuid> {
    Err(Error::NotImplemented("email::add_pdf_password"))
}

pub async fn delete_pdf_password(_conn: &mut PgConnection, _rule_id: Uuid) -> Result<bool> {
    Err(Error::NotImplemented("email::delete_pdf_password"))
}

pub fn register() {
    ownhealth_core::sources::register_poller("email", |ctx, source| Box::pin(poll_mailbox(ctx, source)));
}
