"""People and logins: first-run owner, invites, relatives without a login, claiming, deletion.

Who manages a profile (grants, locks, sensitive switches, export, deletion):
- its `owner`;
- while nobody has claimed the profile (`profile.user_id is null`: a child, an elderly parent),
  a `guardian`. A `caregiver` reads and imports but never manages (security-privacy.md 6.2);
  it may only invite the person to claim their own profile.
"""

from __future__ import annotations

import hashlib
import re
import secrets
import unicodedata
from dataclasses import dataclass
from datetime import date, timedelta
from uuid import UUID, uuid4

from sqlalchemy import Connection, text

from ownhealth.auth import challenges
from ownhealth.core.access import (
    AccessDenied,
    Permission,
    ProfileIn,
    create_household,
    create_profile,
    set_grant,
)

INVITE_TTL = timedelta(days=7)
_SETUP_LOCK = 0x0E_0600  # advisory lock id for first-run setup


class HouseholdError(ValueError):
    def __init__(self, code: str, message: str, **extra):
        super().__init__(message)
        self.code = code
        self.extra = extra


def can_manage(perm: Permission) -> bool:
    return perm.role == "owner" or (perm.role == "guardian" and perm.profile.user_id is None)


def require_manage(perm: Permission) -> Permission:
    if not can_manage(perm):
        raise AccessDenied("forbidden", f"The role {perm.role} cannot manage this profile.")
    return perm


def slugify(name: str) -> str:
    ascii_ = unicodedata.normalize("NFKD", name).encode("ascii", "ignore").decode().lower()
    first = (ascii_.split() or ["person"])[0]
    slug = re.sub(r"[^a-z0-9]+", "-", first).strip("-")[:40] or "person"
    return "person" if slug == "me" else slug


def unique_slug(conn: Connection, household_id: UUID, wanted: str) -> str:
    base = slugify(wanted) if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,39}", wanted or "") or wanted == "me" else wanted
    taken = set(conn.execute(text("select slug from profile where household_id = :h"), {"h": household_id}).scalars())
    slug, n = base, 2
    while slug in taken:
        suffix = f"-{n}"
        slug, n = base[: 40 - len(suffix)] + suffix, n + 1
    return slug


def users_exist(conn: Connection) -> bool:
    return conn.execute(text("select exists (select 1 from app_user)")).scalar_one()


# ------------------------------------------------------------------ first run ----


@dataclass(frozen=True)
class Owner:
    household_id: UUID
    user_id: UUID
    profile_id: UUID
    slug: str


def setup_owner(
    conn: Connection,
    *,
    user_id: UUID | None = None,
    household_name: str,
    display_name: str,
    locale: str = "es",
    oidc_subject: str | None = None,
    birth_date: date | None = None,
    sex_at_birth: str | None = None,
    country: str | None = None,
) -> Owner:
    """Create the household, its first login and that person's own profile, as owner.

    Only while the instance has no user at all; two racing setups serialise on an advisory lock.
    """
    conn.execute(text("select pg_advisory_xact_lock(:k)"), {"k": _SETUP_LOCK})
    if users_exist(conn):
        raise HouseholdError("already_set_up", "This instance already has an owner. Log in instead.")
    display_name = display_name.strip()[:80]
    if not display_name:
        raise HouseholdError("bad_input", "A name is needed.")
    h = create_household(conn, household_name.strip()[:80] or display_name)
    uid = user_id or uuid4()
    conn.execute(
        text("insert into app_user (id, household_id, display_name, locale, oidc_subject) values (:i, :h, :n, :l, :o)"),
        {"i": uid, "h": h, "n": display_name, "l": locale if locale in ("es", "en") else "es", "o": oidc_subject},
    )
    slug = unique_slug(conn, h, display_name)
    pid = create_profile(
        conn,
        ProfileIn(
            household_id=h,
            slug=slug,
            display_name=display_name,
            relationship="self",
            birth_date=birth_date,
            sex_at_birth=sex_at_birth,
            country=country,
            user_id=uid,
        ),
    )
    set_grant(conn, uid, pid, "owner")
    return Owner(h, uid, pid, slug)


# -------------------------------------------------------------------- invites ----


def _code_hash(code: str) -> bytes:
    return hashlib.sha256(code.encode()).digest()


def invite_member(
    conn: Connection,
    *,
    household_id: UUID,
    invited_by: UUID,
    display_name: str,
    locale: str = "es",
    claim_profile_id: UUID | None = None,
) -> tuple[UUID, str]:
    """Create a login that has nothing yet (grants come separately) and a 7-day invite code.

    With `claim_profile_id` the invited person becomes that profile's owner when they enrol.
    """
    display_name = display_name.strip()[:80]
    if not display_name:
        raise HouseholdError("bad_input", "A name is needed.")
    if claim_profile_id is not None:
        claimed = conn.execute(
            text("select user_id from profile where id = :p and household_id = :h"),
            {"p": claim_profile_id, "h": household_id},
        ).first()
        if claimed is None or claimed.user_id is not None:
            raise HouseholdError("already_claimed", "That profile already belongs to a login.")
    uid = conn.execute(
        text("insert into app_user (household_id, display_name, locale) values (:h, :n, :l) returning id"),
        {"h": household_id, "n": display_name, "l": locale if locale in ("es", "en") else "es"},
    ).scalar_one()
    code = secrets.token_urlsafe(24)
    challenges.issue(
        conn,
        "invite",
        _code_hash(code),
        user_id=uid,
        data={"invited_by": str(invited_by), "claim_profile_id": str(claim_profile_id) if claim_profile_id else None},
        ttl=INVITE_TTL,
    )
    return uid, code


def peek_invite(conn: Connection, code: str) -> challenges.Challenge | None:
    return challenges.peek(conn, _code_hash(code), ("invite",))


def redeem_invite(conn: Connection, code: str) -> UUID:
    """Consume the invite; claim its profile if it names one. Returns the invited user's id."""
    try:
        ch = challenges.consume(conn, _code_hash(code), ("invite",))
    except challenges.ChallengeError as exc:
        raise HouseholdError("invite_invalid", "This invite link expired or was already used.") from exc
    claim = ch.data.get("claim_profile_id")
    if claim:
        if not claim_invite_allowed(conn, UUID(claim), UUID(ch.data["invited_by"])):
            raise HouseholdError("invite_invalid", "Someone else manages this profile now. Ask them for a new invite.")
        claim_profile(conn, UUID(claim), ch.user_id, granted_by=UUID(ch.data["invited_by"]))
    return ch.user_id


def claim_invite_allowed(conn: Connection, profile_id: UUID, inviter: UUID) -> bool:
    """A claim invite counts when its sender manages the profile (owner, or guardian of an unclaimed
    one), or when nobody does. A caregiver's own invite must not turn a login they hold into the
    owner of a profile its guardians manage."""
    return bool(
        conn.execute(
            text(
                "select exists (select 1 from profile_grant where profile_id = :p and user_id = :u"
                " and role in ('owner', 'guardian') and (expires_at is null or expires_at > now()))"
                " or not exists (select 1 from profile_grant where profile_id = :p"
                " and role in ('owner', 'guardian') and (expires_at is null or expires_at > now()))"
            ),
            {"p": profile_id, "u": inviter},
        ).scalar()
    )


def claim_profile(conn: Connection, profile_id: UUID, user_id: UUID, *, granted_by: UUID | None = None) -> None:
    """The person takes over their own profile: it is linked to their login and they become owner.

    Existing guardian and caregiver grants stay until the new owner changes them.
    """
    res = conn.execute(
        text("update profile set user_id = :u where id = :p and user_id is null"), {"u": user_id, "p": profile_id}
    )
    if res.rowcount != 1:
        raise HouseholdError("already_claimed", "That profile already belongs to a login.")
    set_grant(conn, user_id, profile_id, "owner", granted_by=granted_by)


def add_relative(
    conn: Connection,
    *,
    household_id: UUID,
    created_by: UUID,
    display_name: str,
    role: str,
    relationship: str | None = None,
    birth_date: date | None = None,
    sex_at_birth: str | None = None,
    country: str | None = None,
    slug: str | None = None,
) -> tuple[UUID, str]:
    """A profile without a login (a child, a parent), managed by its creator as guardian.

    Only guardian: a caregiver cannot manage, so a profile with only a caregiver grant would have
    nobody to grant access, delete it or let its creator close their account."""
    if role != "guardian":
        raise HouseholdError("bad_input", "The creator of a relative's profile is its guardian.")
    display_name = display_name.strip()[:80]
    if not display_name:
        raise HouseholdError("bad_input", "A name is needed.")
    s = unique_slug(conn, household_id, slug or display_name)
    pid = create_profile(
        conn,
        ProfileIn(
            household_id=household_id,
            slug=s,
            display_name=display_name,
            relationship=relationship,
            birth_date=birth_date,
            sex_at_birth=sex_at_birth,
            country=country,
        ),
    )
    set_grant(conn, created_by, pid, role, granted_by=created_by)
    return pid, s


# ------------------------------------------------------------------- deletion ----

# Tables that hold blob hashes, and their columns: a blob is deleted only when no row still names it.
BLOB_COLUMNS: dict[str, tuple[str, ...]] = {
    "document": ("blob_sha256",),
    "document_page": ("words_blob", "image_blob"),
    "genome_dataset": ("facts_blob", "vcf_blob", "tbi_blob", "coverage_blob"),
}
_KEEP = {"profile", "profile_grant", "audit_event"}


def _profile_tables(conn: Connection) -> list[str]:
    """Every table with a profile_id column (other lanes add their own), minus the ones we keep."""
    rows = conn.execute(
        text(
            "select c.table_name from information_schema.columns c join information_schema.tables t"
            " on t.table_schema = c.table_schema and t.table_name = c.table_name"
            " where c.table_schema = current_schema() and c.column_name = 'profile_id'"
            " and t.table_type = 'BASE TABLE' order by c.table_name"
        )
    ).scalars()
    return [t for t in rows if t not in _KEEP]


def _profile_blobs(conn: Connection, profile_id: UUID) -> set[bytes]:
    shas: set[bytes] = set()
    q = {
        "document": "select blob_sha256 from document where profile_id = :p",
        "document_page": "select words_blob, image_blob from document_page dp join document d on d.id = dp.document_id"
        " where d.profile_id = :p",
        "genome_dataset": "select facts_blob, vcf_blob, tbi_blob, coverage_blob from genome_dataset"
        " where profile_id = :p",
    }
    for sql in q.values():
        for row in conn.execute(text(sql), {"p": profile_id}):
            shas.update(bytes(v) for v in row if v is not None)
    return shas


def _still_referenced(conn: Connection, sha: bytes) -> bool:
    for table, cols in BLOB_COLUMNS.items():
        cond = " or ".join(f"{c} = :s" for c in cols)
        if conn.execute(text(f"select 1 from {table} where {cond} limit 1"), {"s": sha}).first():
            return True
    return False


def delete_profile(conn: Connection, profile_id: UUID) -> dict:
    """Hard delete every row of the profile (security-privacy.md 12). Returns counts per table and the
    blob hashes no longer referenced; the caller deletes those files after the transaction commits.

    The audit log keeps its events (ids only, no payloads). Tokens drop the profile; a token left
    with no profile is deleted.
    """
    blob_candidates = _profile_blobs(conn, profile_id)
    # candidate_row -> observation -> src -> candidate_row is a cycle: cut it first.
    conn.execute(
        text(
            "update candidate_row set observation_id = null where observation_id in"
            " (select id from observation where profile_id = :p) or extraction_run_id in"
            " (select r.id from extraction_run r join document d on d.id = r.document_id where d.profile_id = :p)"
        ),
        {"p": profile_id},
    )
    todo, counts = _profile_tables(conn), {}
    while todo:
        failed = []
        for table in todo:
            sp = conn.begin_nested()
            try:
                counts[table] = conn.execute(
                    text(f'delete from "{table}" where profile_id = :p'), {"p": profile_id}
                ).rowcount
                sp.commit()
            except Exception as exc:  # noqa: BLE001 - a foreign key from a table not deleted yet
                sp.rollback()
                if getattr(getattr(exc, "orig", None), "sqlstate", None) != "23503":
                    raise
                failed.append(table)
        if len(failed) == len(todo):
            raise HouseholdError("delete_blocked", f"Rows in {failed} are still referenced.")
        todo = failed
    conn.execute(
        text(
            "update api_token set profile_ids = array_remove(profile_ids, :p),"
            " default_profile = case when default_profile = :p then (array_remove(profile_ids, :p))[1]"
            " else default_profile end where :p = any(profile_ids) or default_profile = :p"
        ),
        {"p": profile_id},
    )
    counts["api_token"] = conn.execute(text("delete from api_token where cardinality(profile_ids) = 0")).rowcount
    conn.execute(text("delete from profile where id = :p"), {"p": profile_id})
    orphans = sorted(sha.hex() for sha in blob_candidates if not _still_referenced(conn, sha))
    return {"rows": {k: v for k, v in counts.items() if v}, "blobs": orphans}


def orphaned_profiles(conn: Connection, user_id: UUID, *, except_profile: UUID | None = None) -> list[str]:
    """Slugs of profiles nobody else could manage once this login closes (no other owner or guardian;
    the user's own profile becomes unclaimed, so a guardian could take over)."""
    return list(
        conn.execute(
            text(
                "select p.slug from profile_grant g join profile p on p.id = g.profile_id where g.user_id = :u"
                " and p.id is distinct from :x and not exists"
                " (select 1 from profile_grant o where o.profile_id = g.profile_id and o.user_id <> :u"
                " and o.role in ('owner', 'guardian')) order by p.slug"
            ),
            {"u": user_id, "x": except_profile},
        ).scalars()
    )


def delete_account(conn: Connection, user_id: UUID) -> dict:
    """Close a login. Reviewed facts keep pointing at the user id (clinical rows are immutable), so the
    user row stays, disabled and renamed; every way in (passkeys, sessions, tokens, codes, OIDC link,
    password, grants) is removed and their own profile becomes unclaimed.

    Refused while a profile would be left with nobody to manage it: delete it or grant someone first.
    """
    orphaned = orphaned_profiles(conn, user_id)
    if orphaned:
        raise HouseholdError(
            "last_manager",
            "Nobody else can reach these profiles. Delete them or grant someone access first.",
            profiles=list(orphaned),
        )
    for sql in (
        "delete from passkey where user_id = :u",
        "delete from recovery_code where user_id = :u",
        "delete from api_token where user_id = :u",
        "delete from profile_grant where user_id = :u",
        "delete from auth_challenge where user_id = :u",
        "update user_session set revoked_at = now() where user_id = :u and revoked_at is null",
        "update profile set user_id = null where user_id = :u",
        "update app_user set disabled_at = now(), display_name = 'Deleted user', oidc_subject = null,"
        " password_hash = null where id = :u",
    ):
        conn.execute(text(sql), {"u": user_id})
    return {"user_id": user_id, "disabled": True}
