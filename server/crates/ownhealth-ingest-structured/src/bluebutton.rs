//! CMS Blue Button v3. The grant names one Patient whose demographics must match the profile.
//! Coverage and ExplanationOfBenefit stay as FHIR in a sealed collection and enter the existing
//! grouped import review. A new pull of the same entries reuses the document and the source ledger.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, NaiveDate, Utc};
use ownhealth_core::arrive::store_bytes;
use ownhealth_core::blobs::BlobOwner;
use ownhealth_core::connector_oauth::{self as oauth, OAuthError, SourceHttp};
use ownhealth_core::documents::{NewDocument, create_document};
use ownhealth_core::jobs::{JobContext, JobError};
use ownhealth_core::net::{Egress, NetError};
use ownhealth_core::secrets::{SecretBox, SecretString};
use ownhealth_core::sources::{RunOutcome, RunProblem, Source};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::fhir::{self, fhir_id, mapping};

pub const KIND: &str = "bluebutton";
const COUNT: usize = 50;
const MAX_PAGES: usize = 200;
const PAGE_BYTES: usize = 16 * 1024 * 1024;
const RUN_BYTES: usize = 128 * 1024 * 1024;
const FHIR_JSON: &str = "application/fhir+json";

const NO_PATIENT: &str = "no_patient: the grant names no patient, so there is nothing to search";
const NO_BASE: &str = "no_server: the connection names no CMS Blue Button v3 server";
const NO_PROFILE: &str = "no_profile: the connection belongs to no profile";
const NO_PATIENT_SCOPE: &str =
    "patient_not_granted: the grant does not cover searching for the patient, so nothing was imported";
const IDENTITY: &str =
    "patient_unverified: the Patient response does not verify the profile's identity, so nothing was imported";
const ANOTHER_PATIENT: &str =
    "another_patient: the account at Medicare is for another person than this profile, so nothing was imported";
const NO_ANSWER: &str = "bluebutton_no_answer: neither Medicare data search could be read";
const NONE_GRANTED: &str = "bluebutton_none_granted: the Medicare sign-in allows neither the coverage nor the claim search, so nothing was read";
const RECONSENT: &str = "reconsent_needed: the vendor no longer accepts this connection, so it waits for a new sign-in";
const REFUSED_ACCESS: &str = "bluebutton_access_refused: CMS refused the renewed access token";
const TOKEN_UNREACHABLE: &str = "token_unreachable: the CMS token request failed";
const GRANT_CHANGED: &str = "grant_changed: the Medicare sign-in changed during the run, so nothing was imported";
const PROFILE_CHANGED: &str =
    "profile_changed: the profile name or birth date changed during the run, so nothing was imported";

fn job_error(e: OAuthError) -> JobError {
    match e {
        OAuthError::ReconsentNeeded => JobError::permanent(RECONSENT),
        OAuthError::GrantChanged => JobError::permanent(GRANT_CHANGED),
        OAuthError::Core(e) => e.into(),
        OAuthError::Refused { code, message } => JobError::permanent(format!("{code}: {message}")),
        _ => JobError::retry(TOKEN_UNREACHABLE),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resource {
    Patient,
    Coverage,
    Claim,
}

impl Resource {
    fn kind(self) -> &'static str {
        match self {
            Self::Patient => "Patient",
            Self::Coverage => "Coverage",
            Self::Claim => "ExplanationOfBenefit",
        }
    }

    fn search(self, base: &str, patient: &str, offset: usize) -> String {
        let mut q = url::form_urlencoded::Serializer::new(String::new());
        match self {
            Self::Patient => q.append_pair("_id", patient),
            Self::Coverage => q.append_pair("beneficiary", &format!("Patient/{patient}")),
            Self::Claim => q.append_pair("patient", patient),
        };
        q.append_pair("_count", &COUNT.to_string());
        q.append_pair("_offset", &offset.to_string());
        format!("{base}/{}/?{}", self.kind(), q.finish())
    }

    fn granted(self, scope: &str) -> bool {
        scope.split_whitespace().any(|s| {
            s.strip_prefix("patient/")
                .and_then(|s| s.split_once('.'))
                .is_some_and(|(kind, permission)| kind == self.kind() && matches!(permission, "read" | "s" | "rs"))
        })
    }

    fn matches(self, resource: &Value, base: &str, patient: &str) -> bool {
        let reference = match self {
            Self::Patient => return resource["id"] == patient,
            Self::Coverage => resource["beneficiary"]["reference"].as_str(),
            Self::Claim => resource["patient"]["reference"].as_str(),
        };
        // No prefix, suffix, version or foreign-base reconciliation for patient references.
        reference.is_some_and(|r| r == format!("Patient/{patient}") || r == format!("{base}/Patient/{patient}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Issue {
    NotGranted,
    Status(u16),
    Request,
    NotBundle,
    BadEntry,
    PatientReference,
    ResourceUrl,
    PageTooLarge,
    RunTooLarge,
    TooManyPages(usize),
    RepeatedPage,
    EmptyPage,
}

impl Issue {
    fn words(&self) -> (&'static str, String) {
        match self {
            Self::NotGranted => (
                "bluebutton_not_granted",
                "the Medicare sign-in did not allow this search".into(),
            ),
            Self::Status(status) => ("search_failed", format!("the server answered {status}")),
            Self::Request => ("bluebutton_request", "the request to CMS failed".into()),
            Self::NotBundle => ("not_fhir", "the answer is not a FHIR search result".into()),
            Self::BadEntry => (
                "unexpected_resource",
                "a result did not match the search and was skipped".into(),
            ),
            Self::PatientReference => (
                "patient_reference",
                "a result did not name the verified patient and was skipped".into(),
            ),
            Self::ResourceUrl => (
                "resource_url",
                "a result named a resource outside the authorized FHIR base and was skipped".into(),
            ),
            Self::PageTooLarge => (
                "page_too_large",
                format!("the answer exceeded the {PAGE_BYTES}-byte limit for one page"),
            ),
            Self::RunTooLarge => (
                "too_large",
                format!("the answers exceeded the {RUN_BYTES}-byte limit for this run"),
            ),
            Self::TooManyPages(pages) => ("too_many_pages", format!("stopped after {pages} pages")),
            Self::RepeatedPage => (
                "repeated_page",
                "the server sent the same results again, so the rest was not read".into(),
            ),
            Self::EmptyPage => (
                "bluebutton_empty_page",
                "CMS returned an empty page with a next page link".into(),
            ),
        }
    }

    fn problem(&self, kind: Resource, times: usize) -> RunProblem {
        let (code, words) = self.words();
        let message = format!("{}: {words}", kind.kind());
        let message = if times > 1 {
            format!("{message} ({times} times)")
        } else {
            message
        };
        RunProblem {
            code: code.into(),
            message,
        }
    }
}

pub fn phrases() -> Value {
    let types = [
        Issue::NotGranted,
        Issue::Status(503),
        Issue::Request,
        Issue::NotBundle,
        Issue::BadEntry,
        Issue::PatientReference,
        Issue::ResourceUrl,
        Issue::PageTooLarge,
        Issue::RunTooLarge,
        Issue::TooManyPages(MAX_PAGES),
        Issue::RepeatedPage,
        Issue::EmptyPage,
    ];
    json!({
        "stop": [NO_PATIENT, NO_BASE, NO_PROFILE, NO_PATIENT_SCOPE, IDENTITY, ANOTHER_PATIENT,
                 NO_ANSWER, NONE_GRANTED, RECONSENT, REFUSED_ACCESS, TOKEN_UNREACHABLE, GRANT_CHANGED, PROFILE_CHANGED],
        "problems": types.iter().map(|issue| issue.problem(Resource::Claim, 1)).collect::<Vec<_>>(),
    })
}

#[derive(Default)]
struct Results {
    // Keep a deterministic order and count every repeated or refused entry by its reason.
    entries: BTreeMap<(String, [u8; 32]), Value>,
    problems: Vec<(Issue, usize)>,
    skipped: BTreeMap<String, i64>,
    answered: bool,
}

impl Results {
    fn problem(&mut self, issue: Issue) {
        if let Some((_, times)) = self.problems.iter_mut().find(|(other, _)| *other == issue) {
            *times += 1;
        } else {
            self.problems.push((issue, 1));
        }
    }

    fn skip(&mut self, code: &str) {
        *self.skipped.entry(code.into()).or_default() += 1;
    }

    fn add(&mut self, kind: Resource, entry: &Value, base: &str, patient: &str) {
        let resource = &entry["resource"];
        let Some(id) = resource["id"].as_str().filter(|s| fhir_id(s)) else {
            self.problem(Issue::BadEntry);
            self.skip("invalid_entry");
            return;
        };
        if resource["resourceType"] != kind.kind()
            || entry
                .get("search")
                .and_then(|s| s.get("mode"))
                .is_some_and(|mode| mode != "match")
        {
            self.problem(Issue::BadEntry);
            self.skip("invalid_entry");
            return;
        }
        let full_url = format!("{base}/{}/{id}", kind.kind());
        // CMS v3 omits fullUrl in some search responses. When supplied, it must be this identity.
        if entry.get("fullUrl").is_some_and(|url| url != &full_url) {
            self.problem(Issue::ResourceUrl);
            self.skip("resource_url");
            return;
        }
        if !kind.matches(resource, base, patient) {
            self.problem(Issue::PatientReference);
            self.skip("patient_reference");
            return;
        }
        // Distinct versions belong in the bundle. The importer keeps its version/sha ledger.
        let key = (id.to_string(), Sha256::digest(resource.to_string()).into());
        if self
            .entries
            .insert(key, json!({"fullUrl": full_url, "resource": resource}))
            .is_some()
        {
            self.skip("repeated");
        }
    }
}

enum SearchError {
    Unauthorized,
    Stopped(JobError),
}

impl From<JobError> for SearchError {
    fn from(error: JobError) -> Self {
        Self::Stopped(error)
    }
}

struct Reader<'a> {
    http: &'a SourceHttp,
    bearer: String,
    base: &'a str,
    patient: &'a str,
    remaining: usize,
    pages: usize,
}

impl Reader<'_> {
    async fn search(&mut self, kind: Resource) -> Result<Results, SearchError> {
        let mut got = Results::default();
        let mut offset = 0;
        let mut seen = BTreeSet::<[u8; 32]>::new();
        for _ in 0..self.pages {
            let limit = self.remaining.min(PAGE_BYTES);
            if limit == 0 {
                got.problem(Issue::RunTooLarge);
                return Ok(got);
            }
            let url = kind.search(self.base, self.patient, offset);
            let headers = [("accept", FHIR_JSON), ("authorization", self.bearer.as_str())];
            let answer = match self.http.get_bounded(&url, &headers, limit).await {
                Ok(answer) => answer,
                Err((e, bytes)) => {
                    self.remaining = self.remaining.saturating_sub(bytes);
                    got.problem(match e {
                        OAuthError::Net(NetError::TooLarge(_)) if limit < PAGE_BYTES => {
                            self.remaining = 0;
                            Issue::RunTooLarge
                        }
                        OAuthError::Net(NetError::TooLarge(_)) => Issue::PageTooLarge,
                        _ => Issue::Request,
                    });
                    return Ok(got);
                }
            };
            self.remaining = self.remaining.saturating_sub(answer.body.len());
            match answer.status {
                200 => {}
                401 => return Err(SearchError::Unauthorized),
                status => {
                    got.problem(Issue::Status(status));
                    return Ok(got);
                }
            }
            let bundle: Value = serde_json::from_slice(&answer.body).unwrap_or_default();
            let mut entries = &[][..];
            let mut page_hash = None;
            if bundle["resourceType"] != "Bundle" || bundle["type"] != "searchset" {
                got.problem(Issue::NotBundle);
                return Ok(got);
            }
            if let Some(value) = bundle.get("entry") {
                let Some(array) = value.as_array() else {
                    got.problem(Issue::NotBundle);
                    return Ok(got);
                };
                if array.len() > COUNT {
                    got.problem(Issue::NotBundle);
                    return Ok(got);
                }
                entries = array.as_slice();
                // the compact JSON of the entry array; Display of a Value cannot fail
                page_hash = Some(<[u8; 32]>::from(Sha256::digest(value.to_string())));
            }
            let next = bundle["link"]
                .as_array()
                .is_some_and(|links| links.iter().any(|l| l["relation"] == "next"));
            if let Some(hash) = page_hash
                && !entries.is_empty()
                && !seen.insert(hash)
            {
                got.problem(Issue::RepeatedPage);
                return Ok(got);
            }
            got.answered = true;
            for entry in entries {
                got.add(kind, entry, self.base, self.patient);
            }
            // Trust the presence of a next relation only. CMS can report the size of this page
            // as total. Build every offset URL ourselves and never contact a URL from the bundle.
            if !next {
                return Ok(got);
            }
            if entries.is_empty() {
                got.problem(Issue::EmptyPage);
                return Ok(got);
            }
            offset += entries.len();
        }
        got.problem(Issue::TooManyPages(self.pages));
        Ok(got)
    }
}

type Identity = (String, Option<NaiveDate>);

async fn gather(r: &mut Reader<'_>, scope: &str, who: &Identity) -> Result<Vec<(Resource, Results)>, SearchError> {
    if !Resource::Patient.granted(scope) {
        return Err(JobError::permanent(NO_PATIENT_SCOPE).into());
    }
    // a retry cannot widen the grant, so it stops for good before any request
    if ![Resource::Coverage, Resource::Claim]
        .iter()
        .any(|kind| kind.granted(scope))
    {
        return Err(JobError::permanent(NONE_GRANTED).into());
    }
    let patients = r.search(Resource::Patient).await?;
    if !patients.problems.is_empty() || patients.entries.len() != 1 || !patients.skipped.is_empty() {
        if let Some((issue, times)) = patients.problems.first() {
            let problem = issue.problem(Resource::Patient, *times);
            return Err(JobError::retry(format!("{}: {}", problem.code, problem.message)).into());
        }
        return Err(JobError::permanent(IDENTITY).into());
    }
    let Some(entry) = patients.entries.values().next() else {
        return Err(JobError::permanent(IDENTITY).into());
    };
    let p = &entry["resource"];
    use mapping::PatientIdentity;
    match mapping::smart_patient_identity(p, &who.0, who.1) {
        PatientIdentity::Verified => {}
        PatientIdentity::Mismatch => return Err(JobError::permanent(ANOTHER_PATIENT).into()),
        PatientIdentity::Unverified => return Err(JobError::permanent(IDENTITY).into()),
    }
    let mut results = vec![(Resource::Patient, patients)];
    for kind in [Resource::Coverage, Resource::Claim] {
        let got = if kind.granted(scope) {
            r.search(kind).await?
        } else {
            let mut missing = Results::default();
            missing.problem(Issue::NotGranted);
            missing.skip("not_granted");
            missing
        };
        results.push((kind, got));
    }
    if results.iter().skip(1).all(|(_, got)| !got.answered) {
        return Err(JobError::retry(NO_ANSWER).into());
    }
    Ok(results)
}

pub async fn pull(
    ctx: &JobContext,
    source: &Source,
    egress: &Egress,
    now: DateTime<Utc>,
    max_pages: usize,
) -> Result<RunOutcome, JobError> {
    let profile = source.profile_id.ok_or_else(|| JobError::permanent(NO_PROFILE))?;
    let base = source.config["endpoints"]["issuer"]
        .as_str()
        .filter(|base| oauth::bluebutton_base(base, egress))
        .ok_or_else(|| JobError::permanent(NO_BASE))?;
    let patient = source.config["patient"]
        .as_str()
        .filter(|id| fhir_id(id))
        .ok_or_else(|| JobError::permanent(NO_PATIENT))?;
    let who: Identity = sqlx::query_as("select display_name, birth_date from profile where id = $1")
        .bind(profile)
        .fetch_optional(&ctx.pool)
        .await?
        .ok_or_else(|| JobError::permanent(PROFILE_CHANGED))?;
    let sb = SecretBox::from_settings(&ctx.settings)?.with_keys(ctx.blobs.keyring());
    let http = SourceHttp::for_source(
        source,
        Egress {
            max_bytes: PAGE_BYTES,
            timeout: Duration::from_secs(60),
            ..egress.clone()
        },
    );
    let mut rejected = None::<SecretString>;
    let mut remaining = RUN_BYTES;
    let resources = loop {
        let token = oauth::access_token_for_grant(&ctx.pool, &sb, source, egress, now, rejected.as_ref())
            .await
            .map_err(job_error)?;
        // Refresh can narrow the granted scopes. Read the committed scope of this grant.
        let scope: Option<String> = sqlx::query_scalar(
            "select config->>'scope' from source where id = $1 and oauth_grant_id is not distinct from $2              and enabled and reconsent_needed_at is null",
        )
        .bind(source.id)
        .bind(source.oauth_grant_id)
        .fetch_optional(&ctx.pool)
        .await?
        .ok_or_else(|| JobError::permanent(GRANT_CHANGED))?;
        let mut reader = Reader {
            http: &http,
            bearer: format!("Bearer {}", token.expose()),
            base,
            patient,
            remaining,
            pages: max_pages,
        };
        match gather(&mut reader, scope.as_deref().unwrap_or_default(), &who).await {
            Ok(results) => break results,
            Err(SearchError::Stopped(error)) => return Err(error),
            Err(SearchError::Unauthorized) if rejected.is_none() => {
                remaining = reader.remaining;
                rejected = Some(token);
            }
            Err(SearchError::Unauthorized) => return Err(JobError::retry(REFUSED_ACCESS)),
        }
    };

    let mut outcome = RunOutcome::default();
    let mut entries = Vec::new();
    for (kind, got) in resources {
        outcome
            .problems
            .extend(got.problems.into_iter().map(|(p, times)| p.problem(kind, times)));
        for (code, n) in got.skipped {
            *outcome.skipped.entry(code).or_default() += n;
        }
        if kind == Resource::Claim {
            // A claim search that did not answer leaves no count, so an older pull's never shows.
            let claims = got.answered.then(|| {
                got.entries
                    .values()
                    .filter_map(|e| e["resource"]["id"].as_str())
                    .collect::<BTreeSet<_>>()
                    .len()
            });
            outcome.cursor = Some(json!({"claim_count": claims}));
        }
        entries.extend(got.entries.into_values());
    }
    let bytes = serde_json::to_vec(&json!({"resourceType": "Bundle", "type": "collection", "entry": entries}))
        .map_err(ownhealth_core::error::Error::from)?;
    let filename = format!("{} · {}.json", source.label, now.format("%Y-%m-%d %H.%M UTC"));
    let mut tx = ctx.pool.begin().await?;
    // Leases can expire. Serialize imports of one source and recheck ownership before any write.
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("source.pull:{}", source.id))
        .execute(&mut *tx)
        .await?;
    let unchanged: Option<bool> = sqlx::query_scalar(
        "select enabled and reconsent_needed_at is null and profile_id = $2            and oauth_grant_id is not distinct from $3 and config->>'patient' = $4            and config#>>'{endpoints,issuer}' = $5 as unchanged from source where id = $1 for share",
    )
    .bind(source.id)
    .bind(profile)
    .bind(source.oauth_grant_id)
    .bind(patient)
    .bind(base)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if unchanged != Some(true) {
        return Err(JobError::permanent(GRANT_CHANGED));
    }
    let current: Option<Identity> =
        sqlx::query_as("select display_name, birth_date from profile where id = $1 for share")
            .bind(profile)
            .fetch_optional(&mut *tx)
            .await?;
    if current.as_ref() != Some(&who) {
        return Err(JobError::permanent(PROFILE_CHANGED));
    }
    let (sha, size) = store_bytes(&ctx.blobs, BlobOwner::Profile(profile), bytes).await?;
    let doc = create_document(
        &mut tx,
        &NewDocument {
            filename: Some(&filename),
            channel: KIND,
            source_id: Some(source.id),
            ..NewDocument::upload(source.household_id, Some(profile), &sha, FHIR_JSON, size as i64)
        },
    )
    .await?;
    let imported = fhir::import_pull_with_identity(&mut tx, &ctx.blobs, source.id, doc.id, Some("matched")).await?;
    tx.commit().await?;
    outcome.skipped.insert("context".into(), imported.context);
    outcome.new = imported.new as i32;
    outcome.duplicate = imported.duplicate as i32;
    outcome.seen = outcome.new + outcome.duplicate;
    Ok(outcome)
}

pub fn register() {
    ownhealth_core::sources::register_poller(KIND, |ctx, source| {
        Box::pin(async move {
            let egress = oauth::connector_egress(ctx.settings.dev_auth);
            pull(&ctx, &source, &egress, Utc::now(), MAX_PAGES).await
        })
    });
}
