//! Households, users, profiles, grants and the one permission function (`core/access.py`).
//!
//! ```text
//! effective = token.scopes ∩ grant(user, profile).categories − profile.locks
//! ```
//!
//! The browser session is a token with every scope. A user with no grant on a profile cannot reach
//! it, whatever a token says. The owner's own grant ignores locks; locks restrict everyone else.
//! Every surface (UI, REST, MCP, export) calls `effective_permission` or `require`.
//!
//! Break-glass (`break_glass.rs`): for a person in the browser, an open emergency request is added to
//! their grant (its categories joined to the grant's, role `viewer` when they hold no grant). Only a
//! REST request that records the uses (`break_glass::arm`) gets it; tokens, OAuth, calendar feeds and
//! share links never do. A read that uses scopes supplied by the request is recorded. A write
//! (`require` with an action, a document move or retraction, a review decision) needs its scope in the
//! caller's own grant (`Permission::own_scopes`, `Permission::own_only`), and exports, feeds and the
//! imaging viewer read with that grant alone (`Permission::own`).

use sqlx::AssertSqlSafe;
use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicI32, Ordering};

use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{Error, Result};

pub const SCOPES: [&str; 10] = [
    "read:summary",
    "read:labs",
    "read:vitals",
    "read:wearables",
    "read:records",
    "read:genome",
    "read:genome_raw",
    "read:sensitive",
    "read:documents_text",
    "write:propose",
];
pub const PAT_DEFAULT_SCOPES: [&str; 3] = ["read:summary", "read:labs", "read:vitals"];
pub const ROLES: [&str; 4] = ["owner", "guardian", "caregiver", "viewer"];
pub const ACTIONS: [&str; 8] = [
    "import",
    "review",
    "write",
    "manage_grants",
    "export",
    "delete",
    "purge",
    "manage_tokens",
];

pub type Scopes = BTreeSet<String>;

pub fn scope_set<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> Scopes {
    items.into_iter().map(|s| s.as_ref().to_string()).collect()
}

pub fn all_scopes() -> Scopes {
    scope_set(SCOPES)
}

pub fn is_scope(s: &str) -> bool {
    SCOPES.contains(&s)
}

/// The categories a new grant of `role` gets.
pub fn default_categories(role: &str) -> Scopes {
    match role {
        "owner" | "guardian" => all_scopes(),
        "caregiver" => scope_set([
            "read:summary",
            "read:labs",
            "read:vitals",
            "read:records",
            "write:propose",
        ]),
        "viewer" => scope_set(["read:summary"]),
        _ => Scopes::new(),
    }
}

/// What each role may do beyond reading. Reading is decided by scopes.
pub fn can(role: &str, action: &str) -> bool {
    let allowed: &[&str] = match role {
        "owner" => &[
            "import",
            "review",
            "write",
            "manage_grants",
            "export",
            "delete",
            "purge",
        ],
        "guardian" | "caregiver" => &["import", "review", "write"],
        _ => &[],
    };
    allowed.contains(&action)
}

/// token scopes ∩ grant categories − profile locks (locks do not bind the owner).
pub fn effective_scopes(token_scopes: &Scopes, categories: &Scopes, locks: &Scopes, role: &str) -> Scopes {
    let both: Scopes = token_scopes.intersection(categories).cloned().collect();
    if role == "owner" {
        both
    } else {
        both.difference(locks).cloned().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PrincipalKind {
    Session,
    Token,
    Dev,
    System,
}

/// Who is calling. A browser session has every scope; a PAT has its own scopes and profiles.
#[derive(Debug, Clone, PartialEq)]
pub struct Principal {
    pub kind: PrincipalKind,
    pub user_id: Option<Uuid>,
    pub household_id: Option<Uuid>,
    pub scopes: Scopes,
    pub token_id: Option<Uuid>,
    pub token_label: Option<String>,
    /// None: every profile the user has a grant on.
    pub profile_ids: Option<BTreeSet<Uuid>>,
    pub default_profile: Option<Uuid>,
    pub identity_mode: String,
    pub lang: String,
    pub units: String,
    /// The session row behind a browser principal (step-up, logout).
    pub session_id: Option<Uuid>,
}

impl Principal {
    /// A browser session (or dev auth) for one user: every scope, every granted profile.
    pub fn session(user_id: Uuid, household_id: Uuid) -> Principal {
        Principal {
            kind: PrincipalKind::Session,
            user_id: Some(user_id),
            household_id: Some(household_id),
            scopes: all_scopes(),
            token_id: None,
            token_label: None,
            profile_ids: None,
            default_profile: None,
            identity_mode: "full".into(),
            lang: "es".into(),
            units: "si".into(),
            session_id: None,
        }
    }

    /// `user` | `token` | `system`, the `actor_kind` of audit rows and src rows.
    pub fn actor_kind(&self) -> &'static str {
        match self.kind {
            PrincipalKind::Token => "token",
            PrincipalKind::System => "system",
            _ => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Profile {
    pub id: Uuid,
    pub household_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub user_id: Option<Uuid>,
    pub relationship: Option<String>,
    pub birth_date: Option<NaiveDate>,
    pub sex_at_birth: Option<String>,
    pub country: Option<String>,
    pub deceased: bool,
    pub unit_pref: String,
    pub genome_sensitive_ui: bool,
    pub genome_sensitive_agents: bool,
    pub locks: Vec<String>,
}

impl Profile {
    pub fn locks(&self) -> Scopes {
        scope_set(&self.locks)
    }

    pub fn age_on(&self, on: NaiveDate) -> Option<i32> {
        let b = self.birth_date?;
        let before_birthday = (on.month(), on.day()) < (b.month(), b.day());
        Some(on.year() - b.year() - i32::from(before_birthday))
    }

    pub fn is_minor(&self, on: NaiveDate, age_of_majority: i32) -> bool {
        self.age_on(on).is_some_and(|a| a < age_of_majority)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Permission {
    pub profile: Profile,
    pub role: String,
    pub scopes: Scopes,
    /// An emergency grant (profile_grant.break_glass, or an open break-glass request added to the
    /// grant): it reads, and it never shares.
    pub break_glass: bool,
    /// Reachable only through an open break-glass request: no grant row, so no token, consent,
    /// calendar feed, export or share link for it.
    pub break_glass_only: bool,
    /// The scopes without emergency access: what the caller's own grant gives, when it is not an
    /// emergency grant row. Empty when only an open request reaches the profile.
    pub own_scopes: Scopes,
    /// Whether a live, non-emergency grant row exists for this profile.
    pub has_ordinary_grant: bool,
    pub emergency: Option<EmergencyPermission>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmergencyPermission {
    usage: crate::break_glass::Use,
    scopes: Scopes,
    added: Scopes,
}

impl Permission {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }

    /// A sensitive fact needs both scopes from one permission source.
    pub fn may_read_sensitive(&self, category_scope: &str) -> bool {
        let has_both = |scopes: &Scopes| scopes.contains(category_scope) && scopes.contains("read:sensitive");
        has_both(&self.scopes)
            && self.emergency.as_ref().is_none_or(|e| {
                has_both(&e.scopes) || (!e.added.contains(category_scope) && !e.added.contains("read:sensitive"))
            })
    }

    /// Record the request if it gave access to this profile or supplied a scope used by the read.
    pub fn record_read(&self, scopes: &[&str]) {
        if let Some(e) = &self.emergency
            && (self.break_glass_only || scopes.iter().any(|s| e.added.contains(*s)))
        {
            crate::break_glass::record(e.usage);
        }
    }

    /// For a read whose response uses the whole set of effective scopes.
    pub fn read_scopes(&self) -> &Scopes {
        if let Some(e) = &self.emergency
            && (self.break_glass_only || !e.added.is_empty())
        {
            crate::break_glass::record(e.usage);
        }
        &self.scopes
    }

    /// Scopes for a reader whose sensitive facts all belong to `category_scope`.
    pub fn read_scopes_for_sensitive(&self, category_scope: &str) -> Cow<'_, Scopes> {
        let scopes = self.read_scopes();
        if scopes.contains("read:sensitive") && !self.may_read_sensitive(category_scope) {
            let mut filtered = scopes.clone();
            filtered.remove("read:sensitive");
            Cow::Owned(filtered)
        } else {
            Cow::Borrowed(scopes)
        }
    }

    pub fn can(&self, action: &str) -> bool {
        !self.break_glass_only && can(&self.role, action)
    }

    /// This permission without emergency access, when it still holds `scope`: what an export, a
    /// calendar feed or the imaging viewer reads with, since none of them may carry what only
    /// break-glass opened.
    pub fn own(&self, scope: &str) -> Option<Permission> {
        self.own_scopes.contains(scope).then(|| self.own_only())
    }

    /// This permission without emergency access: what a change checks its object against, since
    /// emergency access only reads.
    pub fn own_only(&self) -> Permission {
        Permission {
            scopes: self.own_scopes.clone(),
            break_glass: false,
            break_glass_only: false,
            emergency: None,
            ..self.clone()
        }
    }
}

const PROFILE_COLS: &str = "id, household_id, slug, display_name, user_id, relationship, birth_date, sex_at_birth, \
                            country, deceased, unit_pref, genome_sensitive_ui, genome_sensitive_agents, locks";

pub async fn get_profile(conn: &mut PgConnection, profile_id: Uuid) -> Result<Option<Profile>> {
    let sql = format!("select {PROFILE_COLS} from profile where id = $1");
    Ok(sqlx::query_as::<_, Profile>(AssertSqlSafe(sql.as_str()))
        .bind(profile_id)
        .fetch_optional(conn)
        .await?)
}

pub async fn get_profile_by_slug(conn: &mut PgConnection, household_id: Uuid, slug: &str) -> Result<Option<Profile>> {
    let sql = format!("select {PROFILE_COLS} from profile where household_id = $1 and slug = $2");
    Ok(sqlx::query_as::<_, Profile>(AssertSqlSafe(sql.as_str()))
        .bind(household_id)
        .bind(slug)
        .fetch_optional(conn)
        .await?)
}

fn not_granted() -> Error {
    // One message for an unknown id and an ungranted one: a uuid must not confirm a profile exists
    // nor reveal its slug. (resolve_profile_ref echoes a slug only when the caller sent it.)
    Error::access_denied("profile_not_granted", "Profile not available.")
}

/// The permission `principal` has on one profile. `AccessDenied` when there is none.
pub async fn effective_permission(
    conn: &mut PgConnection,
    principal: &Principal,
    profile_id: Uuid,
) -> Result<Permission> {
    let profile = get_profile(&mut *conn, profile_id).await?;
    let (Some(profile), Some(user_id)) = (profile, principal.user_id) else {
        return Err(not_granted());
    };
    if principal
        .profile_ids
        .as_ref()
        .is_some_and(|ids| !ids.contains(&profile_id))
    {
        return Err(not_granted());
    }
    let grant: Option<(String, Vec<String>, bool)> = sqlx::query_as(
        "select role, categories, break_glass from profile_grant where user_id = $1 and profile_id = $2 \
         and (expires_at is null or expires_at > now())",
    )
    .bind(user_id)
    .bind(profile_id)
    .fetch_optional(&mut *conn)
    .await?;
    let open = if union_applies(principal, user_id) {
        crate::break_glass::active(&mut *conn, user_id, Some(profile_id))
            .await?
            .into_iter()
            .next()
    } else {
        None
    };
    combine(principal, profile, grant, open.as_ref(), user_id).ok_or_else(not_granted)
}

/// Whether an open break-glass request of this caller counts: a person in the browser, inside a REST
/// request that records its uses.
fn union_applies(principal: &Principal, user_id: Uuid) -> bool {
    matches!(principal.kind, PrincipalKind::Session | PrincipalKind::Dev)
        && principal.profile_ids.is_none()
        && crate::break_glass::armed_for(user_id)
}

/// Combine the grant row and the open request, keeping their effective scopes distinct.
fn combine(
    principal: &Principal,
    profile: Profile,
    grant: Option<(String, Vec<String>, bool)>,
    open: Option<&(Uuid, Uuid, Vec<String>)>,
    user_id: Uuid,
) -> Option<Permission> {
    let (role, categories, emergency_grant, only) = match (grant, open) {
        (None, None) => return None,
        (Some((role, cats, bg)), _) => (role, scope_set(&cats), bg, false),
        (None, Some(_)) => ("viewer".to_string(), Scopes::new(), false, true),
    };
    let locks = profile.locks();
    let grant_scopes = effective_scopes(&principal.scopes, &categories, &locks, &role);
    let emergency = open.map(|(request_id, _, categories)| {
        let scopes = effective_scopes(&principal.scopes, &scope_set(categories), &locks, &role);
        let added = scopes.difference(&grant_scopes).cloned().collect();
        EmergencyPermission {
            usage: crate::break_glass::Use {
                request_id: *request_id,
                profile_id: profile.id,
                contact: user_id,
            },
            scopes,
            added,
        }
    });
    let mut scopes = grant_scopes.clone();
    if let Some(e) = &emergency {
        scopes.extend(e.added.iter().cloned());
    }
    Some(Permission {
        profile,
        role,
        scopes,
        break_glass: emergency_grant || emergency.is_some(),
        break_glass_only: only,
        own_scopes: if emergency_grant { Scopes::new() } else { grant_scopes },
        has_ordinary_grant: !only && !emergency_grant,
        emergency,
    })
}

#[derive(sqlx::FromRow)]
struct GrantedProfile {
    #[sqlx(flatten)]
    profile: Profile,
    role: String,
    categories: Vec<String>,
    break_glass: bool,
}

/// Every profile the principal may reach, in slug order, with role and effective scopes.
/// Callers record emergency access when a response uses these permissions.
pub async fn accessible_profiles(conn: &mut PgConnection, principal: &Principal) -> Result<Vec<Permission>> {
    let Some(user_id) = principal.user_id else {
        return Ok(Vec::new());
    };
    let cols: Vec<String> = PROFILE_COLS.split(',').map(|c| format!("p.{}", c.trim())).collect();
    let sql = format!(
        "select {}, g.role, g.categories, g.break_glass from profile_grant g join profile p on p.id = g.profile_id \
         where g.user_id = $1 and (g.expires_at is null or g.expires_at > now()) order by p.slug",
        cols.join(", ")
    );
    let rows: Vec<GrantedProfile> = sqlx::query_as(AssertSqlSafe(sql.as_str()))
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await?;
    let in_token = |id: &Uuid| principal.profile_ids.as_ref().is_none_or(|ids| ids.contains(id));
    let open = if union_applies(principal, user_id) {
        crate::break_glass::active(&mut *conn, user_id, None).await?
    } else {
        Vec::new()
    };
    let mut out: Vec<Permission> = Vec::new();
    let mut seen = BTreeSet::new();
    for r in rows.into_iter().filter(|r| in_token(&r.profile.id)) {
        seen.insert(r.profile.id);
        let req = open.iter().find(|o| o.1 == r.profile.id);
        let grant = Some((r.role, r.categories, r.break_glass));
        out.extend(combine(principal, r.profile, grant, req, user_id));
    }
    let before = out.len();
    for req in open.iter().filter(|o| !seen.contains(&o.1)) {
        if let Some(profile) = get_profile(&mut *conn, req.1).await? {
            out.extend(combine(principal, profile, None, Some(req), user_id));
        }
    }
    if out.len() > before {
        out.sort_by(|a, b| a.profile.slug.cmp(&b.profile.slug));
    }
    Ok(out)
}

/// A profile reference: a slug, `"me"`, a uuid, or none (the token's default profile).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProfileRef<'a> {
    Id(Uuid),
    Name(&'a str),
    Default,
}

impl<'a> From<&'a str> for ProfileRef<'a> {
    fn from(s: &'a str) -> Self {
        ProfileRef::Name(s)
    }
}

impl From<Uuid> for ProfileRef<'_> {
    fn from(id: Uuid) -> Self {
        ProfileRef::Id(id)
    }
}

impl<'a> From<Option<&'a str>> for ProfileRef<'a> {
    fn from(s: Option<&'a str>) -> Self {
        s.map_or(ProfileRef::Default, ProfileRef::Name)
    }
}

/// Turn a slug, `me`, a uuid or none (the token's default profile) into a profile id.
pub async fn resolve_profile_ref(conn: &mut PgConnection, principal: &Principal, r: ProfileRef<'_>) -> Result<Uuid> {
    let name = match r {
        ProfileRef::Id(id) => return Ok(id),
        ProfileRef::Name(n) => Some(n),
        ProfileRef::Default => None,
    };
    // a name lookup reads nothing: the permission check that follows records a break-glass use
    let allowed = accessible_profiles(conn, principal).await?;
    let slugs = || {
        allowed
            .iter()
            .filter(|p| p.has_ordinary_grant)
            .map(|p| p.profile.slug.clone())
            .collect()
    };
    if matches!(name, None | Some("") | Some("me")) {
        if name.is_none()
            && let Some(d) = principal.default_profile
        {
            return Ok(d);
        }
        if let Some(own) = allowed
            .iter()
            .find(|p| p.profile.user_id.is_some() && p.profile.user_id == principal.user_id)
        {
            return Ok(own.profile.id);
        }
        if let Some(d) = principal.default_profile {
            return Ok(d);
        }
        return Err(Error::access_denied_with(
            "profile_required",
            "Name a profile.",
            slugs(),
        ));
    }
    let name = name.unwrap_or_default();
    if let Ok(id) = Uuid::parse_str(name) {
        return Ok(id);
    }
    if let Some(p) = allowed.iter().find(|p| p.profile.slug == name) {
        return Ok(p.profile.id);
    }
    Err(Error::access_denied_with(
        "profile_not_granted",
        format!("Profile '{name}' is not available."),
        slugs(),
    ))
}

/// Resolve the profile and check one scope and/or one role action. `AccessDenied` otherwise.
pub async fn require(
    conn: &mut PgConnection,
    principal: &Principal,
    profile: ProfileRef<'_>,
    scope: Option<&str>,
    action: Option<&str>,
) -> Result<Permission> {
    let profile_id = resolve_profile_ref(&mut *conn, principal, profile).await?;
    let perm = effective_permission(conn, principal, profile_id).await?;
    if let Some(scope) = scope
        && !perm.has(scope)
    {
        return Err(Error::access_denied(
            "insufficient_scope",
            format!("This needs {scope}."),
        ));
    }
    if let Some(action) = action
        && !perm.can(action)
    {
        return Err(Error::access_denied(
            "forbidden",
            format!("The role {} cannot {action} on this profile.", perm.role),
        ));
    }
    // emergency access only reads: an action needs its scope from the caller's own grant
    if let (Some(scope), Some(_)) = (scope, action)
        && !perm.own_scopes.contains(scope)
    {
        return Err(Error::access_denied(
            "break_glass",
            format!("Emergency access only reads; this needs {scope} on your own grant."),
        ));
    }
    if action.is_none() {
        match scope {
            Some(scope) => perm.record_read(&[scope]),
            None => perm.record_read(&[]),
        }
    }
    Ok(perm)
}

// ------------------------------------------------------------------- writes ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileIn {
    pub household_id: Uuid,
    pub slug: String,
    pub display_name: String,
    pub relationship: Option<String>,
    pub birth_date: Option<NaiveDate>,
    /// female | male | unknown
    pub sex_at_birth: Option<String>,
    pub country: Option<String>,
    pub user_id: Option<Uuid>,
    /// si | conventional | as_reported (default si)
    pub unit_pref: Option<String>,
}

pub async fn create_household(conn: &mut PgConnection, name: &str) -> Result<Uuid> {
    Ok(
        sqlx::query_scalar("insert into household (name) values ($1) returning id")
            .bind(name)
            .fetch_one(conn)
            .await?,
    )
}

pub async fn create_user(
    conn: &mut PgConnection,
    household_id: Uuid,
    display_name: &str,
    locale: &str,
    oidc_subject: Option<&str>,
) -> Result<Uuid> {
    // clock_timestamp, not now(): users made in one transaction keep the order they were made in
    Ok(sqlx::query_scalar(
        "insert into app_user (household_id, display_name, locale, oidc_subject, created_at) \
         values ($1, $2, $3, $4, clock_timestamp()) returning id",
    )
    .bind(household_id)
    .bind(display_name)
    .bind(locale)
    .bind(oidc_subject)
    .fetch_one(conn)
    .await?)
}

pub async fn create_profile(conn: &mut PgConnection, data: &ProfileIn) -> Result<Uuid> {
    if let Some(sex) = &data.sex_at_birth
        && !["female", "male", "unknown"].contains(&sex.as_str())
    {
        return Err(Error::Invalid(format!("sex_at_birth: unknown value {sex}")));
    }
    let unit_pref = data.unit_pref.as_deref().unwrap_or("si");
    if !["si", "conventional", "as_reported"].contains(&unit_pref) {
        return Err(Error::Invalid(format!("unit_pref: unknown value {unit_pref}")));
    }
    Ok(sqlx::query_scalar(
        "insert into profile (household_id, slug, display_name, relationship, birth_date, sex_at_birth, country, \
         user_id, unit_pref) values ($1, $2, $3, $4, $5, $6, $7, $8, $9) returning id",
    )
    .bind(data.household_id)
    .bind(&data.slug)
    .bind(&data.display_name)
    .bind(&data.relationship)
    .bind(data.birth_date)
    .bind(&data.sex_at_birth)
    .bind(&data.country)
    .bind(data.user_id)
    .bind(unit_pref)
    .fetch_one(conn)
    .await?)
}

/// Create or replace the one grant row for (user, profile). Categories default by role.
pub async fn set_grant(
    conn: &mut PgConnection,
    user_id: Uuid,
    profile_id: Uuid,
    role: &str,
    categories: Option<&Scopes>,
    granted_by: Option<Uuid>,
    expires_at: Option<DateTime<Utc>>,
) -> Result<()> {
    if !ROLES.contains(&role) {
        return Err(Error::Invalid(format!("unknown role {role}")));
    }
    let cats: Vec<String> = categories
        .cloned()
        .unwrap_or_else(|| default_categories(role))
        .into_iter()
        .collect();
    let unknown: Vec<&String> = cats.iter().filter(|c| !is_scope(c)).collect();
    if !unknown.is_empty() {
        return Err(Error::Invalid(format!("unknown categories {unknown:?}")));
    }
    sqlx::query(
        "insert into profile_grant (user_id, profile_id, role, categories, granted_by, expires_at) \
         values ($1, $2, $3, $4, $5, $6) on conflict (user_id, profile_id) do update set \
         role = excluded.role, categories = excluded.categories, granted_by = excluded.granted_by, \
         expires_at = excluded.expires_at, granted_at = now()",
    )
    .bind(user_id)
    .bind(profile_id)
    .bind(role)
    .bind(&cats)
    .bind(granted_by)
    .bind(expires_at)
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn revoke_grant(conn: &mut PgConnection, user_id: Uuid, profile_id: Uuid) -> Result<bool> {
    let n = sqlx::query("delete from profile_grant where user_id = $1 and profile_id = $2")
        .bind(user_id)
        .bind(profile_id)
        .execute(conn)
        .await?
        .rows_affected();
    Ok(n > 0)
}

/// Every grant in a household, by user name then slug (`list_grants`).
pub async fn list_grants(
    conn: &mut PgConnection,
    household_id: Uuid,
) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
    let rows = sqlx::query(
        "select g.user_id, u.display_name as user_name, g.profile_id, p.slug, g.role, g.categories, g.expires_at, \
         g.granted_at from profile_grant g join profile p on p.id = g.profile_id join app_user u on u.id = g.user_id \
         where p.household_id = $1 order by u.display_name, p.slug",
    )
    .bind(household_id)
    .fetch_all(conn)
    .await?;
    rows.iter().map(crate::pyjson::row_to_json).collect()
}

pub async fn set_locks(conn: &mut PgConnection, profile_id: Uuid, locks: &Scopes) -> Result<()> {
    let unknown: Vec<&String> = locks.iter().filter(|c| !is_scope(c)).collect();
    if !unknown.is_empty() {
        return Err(Error::Invalid(format!("unknown scopes {unknown:?}")));
    }
    let sorted: Vec<&String> = locks.iter().collect();
    sqlx::query("update profile set locks = $1 where id = $2")
        .bind(sorted)
        .bind(profile_id)
        .execute(conn)
        .await?;
    Ok(())
}

pub async fn set_genome_switches(
    conn: &mut PgConnection,
    profile_id: Uuid,
    ui: Option<bool>,
    agents: Option<bool>,
) -> Result<()> {
    sqlx::query(
        "update profile set genome_sensitive_ui = coalesce($1, genome_sensitive_ui), \
         genome_sensitive_agents = coalesce($2, genome_sensitive_agents) where id = $3",
    )
    .bind(ui)
    .bind(agents)
    .bind(profile_id)
    .execute(conn)
    .await?;
    Ok(())
}

/// `OWNHEALTH_AGE_OF_MAJORITY` as the running process read it: `Runtime` stores it here when it is
/// built, so code without the settings at hand (the rules engine) answers like the routes.
static ENV_MAJORITY: AtomicI32 = AtomicI32::new(18);

pub fn set_env_majority(years: i32) {
    ENV_MAJORITY.store(years, Ordering::Relaxed);
}

pub fn env_majority() -> i32 {
    ENV_MAJORITY.load(Ordering::Relaxed)
}

/// The age of majority of one household: its `age_of_majority` setting, else
/// `OWNHEALTH_AGE_OF_MAJORITY`, else 18. Every minor check (the profile list, the genome page, the
/// agent's gate, the rules engine, a claim) asks this one function, so they always agree.
pub async fn majority(conn: &mut PgConnection, household_id: Uuid) -> Result<i32> {
    let value: Option<serde_json::Value> =
        sqlx::query_scalar("select value from household_setting where household_id = $1 and key = 'age_of_majority'")
            .bind(household_id)
            .fetch_optional(conn)
            .await?;
    let default = env_majority();
    Ok(match value {
        Some(serde_json::Value::Number(n)) => n.as_i64().and_then(|v| i32::try_from(v).ok()).unwrap_or(default),
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(default),
        _ => default,
    })
}

/// The day every minor check is made on: the UTC day, the one the rules engine, the status page and
/// the agent's default `as_of` use, so a record stored a moment ago is in it. The profile list, the
/// genome page and its consent, the agent's gate and a claim all ask this, so on a birthday they flip
/// together, whatever the server's `TZ`.
pub fn today() -> NaiveDate {
    today_utc()
}

pub fn today_utc() -> NaiveDate {
    Utc::now().date_naive()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_scopes_formula() {
        let token = scope_set(["read:summary", "read:labs", "read:sensitive"]);
        let cats = scope_set(["read:summary", "read:labs", "read:sensitive", "read:genome"]);
        let locks = scope_set(["read:sensitive"]);
        assert_eq!(
            effective_scopes(&token, &cats, &locks, "caregiver"),
            scope_set(["read:summary", "read:labs"])
        );
        // the owner's own grant ignores locks
        assert_eq!(
            effective_scopes(&token, &cats, &locks, "owner"),
            scope_set(["read:summary", "read:labs", "read:sensitive"])
        );
        // token scopes cap everything
        assert_eq!(
            effective_scopes(&scope_set(["read:summary"]), &all_scopes(), &Scopes::new(), "owner"),
            scope_set(["read:summary"])
        );
    }

    #[test]
    fn role_actions() {
        assert!(can("owner", "manage_grants") && can("owner", "export"));
        assert!(can("guardian", "import") && !can("guardian", "export"));
        assert!(can("caregiver", "review") && !can("caregiver", "manage_grants"));
        assert!(!can("viewer", "import") && !can("viewer", "write"));
        assert!(!can("owner", "manage_tokens"));
    }
}
