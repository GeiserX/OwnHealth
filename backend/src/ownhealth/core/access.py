"""Households, users, profiles, grants and the one permission function.

    effective = token.scopes ∩ grant(user, profile).categories − profile.locks

The browser session is a token with every scope. A user with no grant on a profile cannot reach
it, whatever a token says. The owner's own grant ignores locks; locks restrict everyone else.
Every surface (UI, REST, MCP, export) calls `effective_permission` or `require`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from typing import Literal
from uuid import UUID

from pydantic import BaseModel
from sqlalchemy import Connection, text

SCOPES: tuple[str, ...] = (
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
)
ALL_SCOPES: frozenset[str] = frozenset(SCOPES)
PAT_DEFAULT_SCOPES: tuple[str, ...] = ("read:summary", "read:labs", "read:vitals")

Role = Literal["owner", "guardian", "caregiver", "viewer"]
ROLES: tuple[str, ...] = ("owner", "guardian", "caregiver", "viewer")

ROLE_DEFAULT_CATEGORIES: dict[str, frozenset[str]] = {
    "owner": ALL_SCOPES,
    "guardian": ALL_SCOPES,
    "caregiver": frozenset({"read:summary", "read:labs", "read:vitals", "read:records", "write:propose"}),
    "viewer": frozenset({"read:summary"}),
}

# What each role may do beyond reading. Reading is decided by scopes.
Action = Literal["import", "review", "write", "manage_grants", "export", "delete", "purge", "manage_tokens"]
ROLE_ACTIONS: dict[str, frozenset[str]] = {
    "owner": frozenset({"import", "review", "write", "manage_grants", "export", "delete", "purge"}),
    "guardian": frozenset({"import", "review", "write"}),
    "caregiver": frozenset({"import", "review", "write"}),
    "viewer": frozenset(),
}


class AccessDenied(PermissionError):
    """No grant, a token not allowed on the profile, or a missing scope. `code` is stable."""

    def __init__(self, code: str, message: str, allowed: list[str] | None = None):
        super().__init__(message)
        self.code = code
        self.allowed = allowed or []


@dataclass(frozen=True)
class Principal:
    """Who is calling. A browser session has every scope; a PAT has its own scopes and profiles."""

    kind: Literal["session", "token", "dev", "system"]
    user_id: UUID | None
    household_id: UUID | None
    scopes: frozenset[str] = ALL_SCOPES
    token_id: UUID | None = None
    token_label: str | None = None
    profile_ids: frozenset[UUID] | None = None  # None: every profile the user has a grant on
    default_profile: UUID | None = None
    identity_mode: str = "full"
    lang: str = "es"
    units: str = "si"
    extra: dict = field(default_factory=dict)

    @property
    def actor_kind(self) -> str:
        return "token" if self.kind == "token" else ("system" if self.kind == "system" else "user")


@dataclass(frozen=True)
class Profile:
    id: UUID
    household_id: UUID
    slug: str
    display_name: str
    user_id: UUID | None
    relationship: str | None
    birth_date: date | None
    sex_at_birth: str | None
    country: str | None
    deceased: bool
    unit_pref: str
    genome_sensitive_ui: bool
    genome_sensitive_agents: bool
    locks: frozenset[str]

    def age_on(self, on: date) -> int | None:
        if not self.birth_date:
            return None
        b = self.birth_date
        return on.year - b.year - ((on.month, on.day) < (b.month, b.day))

    def is_minor(self, on: date, age_of_majority: int = 18) -> bool:
        age = self.age_on(on)
        return age is not None and age < age_of_majority


@dataclass(frozen=True)
class Permission:
    profile: Profile
    role: str
    scopes: frozenset[str]

    def has(self, scope: str) -> bool:
        return scope in self.scopes

    def can(self, action: str) -> bool:
        return can(self.role, action)


# ----------------------------------------------------------------- pure rules ----


def effective_scopes(
    token_scopes: frozenset[str] | set[str],
    categories: frozenset[str] | set[str],
    locks: frozenset[str] | set[str],
    role: str,
) -> frozenset[str]:
    """token scopes ∩ grant categories − profile locks (locks do not bind the owner)."""
    result = frozenset(token_scopes) & frozenset(categories)
    if role != "owner":
        result -= frozenset(locks)
    return result


def can(role: str, action: str) -> bool:
    return action in ROLE_ACTIONS.get(role, frozenset())


def default_categories(role: str) -> frozenset[str]:
    return ROLE_DEFAULT_CATEGORIES[role]


# -------------------------------------------------------------------- reads ----

_PROFILE_COLS = (
    "id, household_id, slug, display_name, user_id, relationship, birth_date, sex_at_birth, country, deceased,"
    " unit_pref, genome_sensitive_ui, genome_sensitive_agents, locks"
)


def _profile(row) -> Profile:
    d = dict(row._mapping) if hasattr(row, "_mapping") else dict(row)
    d["locks"] = frozenset(d["locks"] or ())
    return Profile(**{k: d[k] for k in Profile.__dataclass_fields__})


def get_profile(conn: Connection, profile_id: UUID) -> Profile | None:
    row = conn.execute(text(f"select {_PROFILE_COLS} from profile where id = :id"), {"id": profile_id}).first()
    return _profile(row) if row else None


def get_profile_by_slug(conn: Connection, household_id: UUID, slug: str) -> Profile | None:
    row = conn.execute(
        text(f"select {_PROFILE_COLS} from profile where household_id = :h and slug = :s"),
        {"h": household_id, "s": slug},
    ).first()
    return _profile(row) if row else None


def _grant(conn: Connection, user_id: UUID, profile_id: UUID):
    return conn.execute(
        text(
            "select role, categories, expires_at from profile_grant where user_id = :u and profile_id = :p"
            " and (expires_at is null or expires_at > now())"
        ),
        {"u": user_id, "p": profile_id},
    ).first()


def effective_permission(conn: Connection, principal: Principal, profile_id: UUID) -> Permission:
    """The permission `principal` has on one profile. Raises AccessDenied when there is none."""
    profile = get_profile(conn, profile_id)
    if profile is None or principal.user_id is None:
        raise AccessDenied("profile_not_granted", "Profile not available.")
    if principal.profile_ids is not None and profile_id not in principal.profile_ids:
        raise AccessDenied("profile_not_granted", f"Profile '{profile.slug}' is not available to this token.")
    grant = _grant(conn, principal.user_id, profile_id)
    if grant is None:
        raise AccessDenied("profile_not_granted", f"Profile '{profile.slug}' is not available.")
    scopes = effective_scopes(principal.scopes, frozenset(grant.categories), profile.locks, grant.role)
    return Permission(profile=profile, role=grant.role, scopes=scopes)


def accessible_profiles(conn: Connection, principal: Principal) -> list[Permission]:
    """Every profile the principal may reach, in slug order, with role and effective scopes."""
    if principal.user_id is None:
        return []
    rows = conn.execute(
        text(
            f"select {', '.join('p.' + c.strip() for c in _PROFILE_COLS.split(','))}, g.role, g.categories"
            " from profile_grant g join profile p on p.id = g.profile_id"
            " where g.user_id = :u and (g.expires_at is null or g.expires_at > now()) order by p.slug"
        ),
        {"u": principal.user_id},
    ).mappings()
    out = []
    for r in rows:
        if principal.profile_ids is not None and r["id"] not in principal.profile_ids:
            continue
        profile = _profile(r)
        scopes = effective_scopes(principal.scopes, frozenset(r["categories"]), profile.locks, r["role"])
        out.append(Permission(profile=profile, role=r["role"], scopes=scopes))
    return out


def resolve_profile_ref(conn: Connection, principal: Principal, ref: str | UUID | None) -> UUID:
    """Turn a slug, 'me', a uuid or None (the token's default profile) into a profile id."""
    if isinstance(ref, UUID):
        return ref
    allowed = accessible_profiles(conn, principal)
    if ref in (None, "", "me"):
        if ref is None and principal.default_profile:
            return principal.default_profile
        for perm in allowed:
            if perm.profile.user_id == principal.user_id:
                return perm.profile.id
        if principal.default_profile:
            return principal.default_profile
        raise AccessDenied("profile_required", "Name a profile.", [p.profile.slug for p in allowed])
    try:
        return UUID(str(ref))
    except ValueError:
        pass
    for perm in allowed:
        if perm.profile.slug == ref:
            return perm.profile.id
    raise AccessDenied(
        "profile_not_granted",
        f"Profile '{ref}' is not available.",
        [p.profile.slug for p in allowed],
    )


def require(
    conn: Connection,
    principal: Principal,
    profile_ref: str | UUID | None,
    scope: str | None = None,
    action: str | None = None,
) -> Permission:
    """Resolve the profile and check one scope and/or one role action. Raises AccessDenied."""
    profile_id = resolve_profile_ref(conn, principal, profile_ref)
    perm = effective_permission(conn, principal, profile_id)
    if scope is not None and scope not in perm.scopes:
        raise AccessDenied("insufficient_scope", f"This needs {scope}.")
    if action is not None and not perm.can(action):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot {action} on this profile.")
    return perm


# ------------------------------------------------------------------- writes ----


class ProfileIn(BaseModel):
    household_id: UUID
    slug: str
    display_name: str
    relationship: str | None = None
    birth_date: date | None = None
    sex_at_birth: Literal["female", "male", "unknown"] | None = None
    country: str | None = None
    user_id: UUID | None = None
    unit_pref: Literal["si", "conventional", "as_reported"] = "si"


def create_household(conn: Connection, name: str) -> UUID:
    return conn.execute(text("insert into household (name) values (:n) returning id"), {"n": name}).scalar_one()


def create_user(
    conn: Connection, household_id: UUID, display_name: str, locale: str = "es", oidc_subject: str | None = None
) -> UUID:
    return conn.execute(
        text(
            "insert into app_user (household_id, display_name, locale, oidc_subject)"
            " values (:h, :n, :l, :o) returning id"
        ),
        {"h": household_id, "n": display_name, "l": locale, "o": oidc_subject},
    ).scalar_one()


def create_profile(conn: Connection, data: ProfileIn) -> UUID:
    return conn.execute(
        text(
            "insert into profile (household_id, slug, display_name, relationship, birth_date, sex_at_birth,"
            " country, user_id, unit_pref) values (:household_id, :slug, :display_name, :relationship,"
            " :birth_date, :sex_at_birth, :country, :user_id, :unit_pref) returning id"
        ),
        data.model_dump(),
    ).scalar_one()


def set_grant(
    conn: Connection,
    user_id: UUID,
    profile_id: UUID,
    role: str,
    categories: frozenset[str] | set[str] | None = None,
    granted_by: UUID | None = None,
    expires_at: datetime | None = None,
) -> None:
    """Create or replace the one grant row for (user, profile). Categories default by role."""
    if role not in ROLES:
        raise ValueError(f"unknown role {role}")
    cats = sorted(categories if categories is not None else default_categories(role))
    unknown = set(cats) - ALL_SCOPES
    if unknown:
        raise ValueError(f"unknown categories {sorted(unknown)}")
    conn.execute(
        text(
            "insert into profile_grant (user_id, profile_id, role, categories, granted_by, expires_at)"
            " values (:u, :p, :r, :c, :g, :e) on conflict (user_id, profile_id) do update set"
            " role = excluded.role, categories = excluded.categories, granted_by = excluded.granted_by,"
            " expires_at = excluded.expires_at, granted_at = now()"
        ),
        {"u": user_id, "p": profile_id, "r": role, "c": cats, "g": granted_by, "e": expires_at},
    )


def revoke_grant(conn: Connection, user_id: UUID, profile_id: UUID) -> bool:
    res = conn.execute(
        text("delete from profile_grant where user_id = :u and profile_id = :p"), {"u": user_id, "p": profile_id}
    )
    return res.rowcount > 0


def list_grants(conn: Connection, household_id: UUID) -> list[dict]:
    rows = conn.execute(
        text(
            "select g.user_id, u.display_name as user_name, g.profile_id, p.slug, g.role, g.categories,"
            " g.expires_at, g.granted_at from profile_grant g join profile p on p.id = g.profile_id"
            " join app_user u on u.id = g.user_id where p.household_id = :h order by u.display_name, p.slug"
        ),
        {"h": household_id},
    ).mappings()
    return [dict(r) for r in rows]


def set_locks(conn: Connection, profile_id: UUID, locks: set[str] | frozenset[str]) -> None:
    unknown = set(locks) - ALL_SCOPES
    if unknown:
        raise ValueError(f"unknown scopes {sorted(unknown)}")
    conn.execute(text("update profile set locks = :l where id = :p"), {"l": sorted(locks), "p": profile_id})


def set_genome_switches(
    conn: Connection, profile_id: UUID, *, ui: bool | None = None, agents: bool | None = None
) -> None:
    conn.execute(
        text(
            "update profile set genome_sensitive_ui = coalesce(:ui, genome_sensitive_ui),"
            " genome_sensitive_agents = coalesce(:ag, genome_sensitive_agents) where id = :p"
        ),
        {"ui": ui, "ag": agents, "p": profile_id},
    )


def age_of_majority(conn: Connection, household_id: UUID, default: int = 18) -> int:
    value = conn.execute(
        text("select value from household_setting where household_id = :h and key = 'age_of_majority'"),
        {"h": household_id},
    ).scalar()
    return int(value) if value is not None else default


def today_utc() -> date:
    return datetime.now(UTC).date()
