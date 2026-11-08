"""Household: members, relatives without a login, invites, grants, locks, sensitive switches, deletion.

Changes that widen access (a grant, removing a lock, turning a sensitive switch on) and deletions need
step-up; changes that only narrow access do not.
"""

from __future__ import annotations

from datetime import date, datetime
from typing import Literal
from uuid import UUID

from fastapi import APIRouter, Depends, HTTPException, Request, Response
from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn, get_runtime, get_settings
from ownhealth.api.core import summaries
from ownhealth.auth import household, sessions
from ownhealth.auth.deps import SessionOnly
from ownhealth.config import Settings
from ownhealth.core.access import (
    ALL_SCOPES,
    Permission,
    Principal,
    accessible_profiles,
    effective_permission,
    require,
    revoke_grant,
    set_genome_switches,
    set_grant,
    set_locks,
)
from ownhealth.core.audit import AuditEvent, write_audit
from ownhealth.runtime import Runtime

router = APIRouter(tags=["family"])


def _fail(exc: household.HouseholdError) -> HTTPException:
    status = 409 if exc.code in ("already_claimed", "last_manager", "delete_blocked") else 400
    return HTTPException(status, detail={"code": exc.code, "message": str(exc), **exc.extra})


def _audit(conn: Connection, principal: Principal, action: str, tool: str, profile_id: UUID | None, **kw) -> None:
    write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=principal.user_id,
            via="ui",
            action=action,
            tool=tool,
            profile_id=profile_id,
            **kw,
        ),
    )


def _manage(conn: Connection, principal: Principal, ref: str) -> Permission:
    return household.require_manage(require(conn, principal, ref))


# ------------------------------------------------------------------- read ----


@router.get("/household")
def get_household(
    conn: Connection = Depends(get_conn), principal: Principal = SessionOnly, settings: Settings = Depends(get_settings)
) -> dict:
    perms = accessible_profiles(conn, principal)
    managed = {p.profile.id for p in perms if household.can_manage(p)}
    members = conn.execute(
        text(
            "select u.id, u.display_name, u.locale, u.created_at,"
            " (select count(*) from passkey k where k.user_id = u.id) as passkeys,"
            " u.oidc_subject is not null as oidc, u.password_hash is not null as password,"
            " exists (select 1 from auth_challenge c where c.user_id = u.id and c.purpose = 'invite'"
            "   and c.used_at is null and c.expires_at > now()) as invite_pending,"
            " (select p.slug from profile p where p.user_id = u.id) as own_profile"
            " from app_user u where u.household_id = :h and u.disabled_at is null order by u.created_at, u.id"
        ),
        {"h": principal.household_id},
    ).mappings()
    grants = conn.execute(
        text(
            "select g.user_id, p.slug, g.role, g.categories, g.expires_at, g.granted_at from profile_grant g"
            " join profile p on p.id = g.profile_id where p.household_id = :h"
            " and (g.profile_id = any(:managed) or g.user_id = :me) order by p.slug, g.user_id"
        ),
        {"h": principal.household_id, "managed": list(managed), "me": principal.user_id},
    ).mappings()
    profiles = summaries(perms, principal, settings)
    for row in profiles:
        row["can_manage"] = row["id"] in managed
        row["claimed"] = next(p.profile.user_id is not None for p in perms if p.profile.id == row["id"])
        row["locks"] = sorted(next(p.profile.locks for p in perms if p.profile.id == row["id"]))
        row["genome_sensitive_agents"] = next(
            p.profile.genome_sensitive_agents for p in perms if p.profile.id == row["id"]
        )
    return {
        "members": [
            {
                "user_id": m["id"],
                "display_name": m["display_name"],
                "locale": m["locale"],
                "logins": {"passkeys": m["passkeys"], "oidc": m["oidc"], "password": m["password"]},
                "enrolled": bool(m["passkeys"] or m["oidc"] or m["password"]),
                "invite_pending": m["invite_pending"],
                "own_profile": m["own_profile"],
                "is_me": m["id"] == principal.user_id,
            }
            for m in members
        ],
        "profiles": profiles,
        "grants": [
            {
                "user_id": g["user_id"],
                "profile": g["slug"],
                "role": g["role"],
                "categories": sorted(g["categories"]),
                "expires_at": g["expires_at"],
                "granted_at": g["granted_at"],
            }
            for g in grants
        ],
    }


# ---------------------------------------------------------- people, invites ----


class RelativeIn(BaseModel):
    display_name: str = Field(min_length=1, max_length=80)
    role: Literal["guardian", "caregiver"]
    relationship: Literal["partner", "child", "parent", "other"] | None = None
    birth_date: date | None = None
    sex_at_birth: Literal["female", "male", "unknown"] | None = None
    country: str | None = Field(None, min_length=2, max_length=2)
    slug: str | None = Field(None, max_length=40)


@router.post("/household/profiles", status_code=201)
def add_relative(body: RelativeIn, conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    """A profile without a login (a child, a parent). Its creator is its guardian."""
    try:
        pid, slug = household.add_relative(
            conn, household_id=principal.household_id, created_by=principal.user_id, **body.model_dump()
        )
    except household.HouseholdError as exc:
        raise _fail(exc) from exc
    _audit(
        conn, principal, "grant_change", "POST /api/household/profiles", pid, args_summary=f"create;role={body.role}"
    )
    return {"id": pid, "slug": slug}


class InviteIn(BaseModel):
    display_name: str = Field(min_length=1, max_length=80)
    locale: Literal["es", "en"] = "es"
    claim_profile: str | None = None


@router.post("/household/invites", status_code=201)
def invite(
    body: InviteIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    settings: Settings = Depends(get_settings),
) -> dict:
    """A login for another person, with a one-time link valid 7 days. With `claim_profile` (a
    profile nobody has claimed yet) that person becomes its owner when they enrol."""
    perms = accessible_profiles(conn, principal)
    claim_id = None
    if body.claim_profile:
        perm = require(conn, principal, body.claim_profile)
        if not (household.can_manage(perm) or perm.role == "caregiver"):
            raise HTTPException(403, detail={"code": "forbidden", "message": "You do not manage this profile."})
        if not household.claim_invite_allowed(conn, perm.profile.id, principal.user_id):
            raise HTTPException(
                403, detail={"code": "forbidden", "message": "A guardian manages this profile: they send the invite."}
            )
        claim_id = perm.profile.id
    elif not any(p.role == "owner" for p in perms):
        raise HTTPException(403, detail={"code": "forbidden", "message": "Only an owner can invite a member."})
    try:
        uid, code = household.invite_member(
            conn,
            household_id=principal.household_id,
            invited_by=principal.user_id,
            display_name=body.display_name,
            locale=body.locale,
            claim_profile_id=claim_id,
        )
    except household.HouseholdError as exc:
        raise _fail(exc) from exc
    _audit(
        conn,
        principal,
        "invite",
        "POST /api/household/invites",
        claim_id,
        resource_type="user",
        resource_ids=[str(uid)],
    )
    return {
        "user_id": uid,
        "code": code,
        "url": f"{settings.base_url}/enroll?code={code}",
        "expires_in_days": household.INVITE_TTL.days,
    }


# --------------------------------------------------------------------- grants ----


class GrantIn(BaseModel):
    user_id: UUID
    profile: str
    role: Literal["guardian", "caregiver", "viewer"]
    categories: list[str] | None = None
    expires_at: datetime | None = None


def _target_user(conn: Connection, principal: Principal, user_id: UUID) -> None:
    ok = conn.execute(
        text("select 1 from app_user where id = :u and household_id = :h and disabled_at is null"),
        {"u": user_id, "h": principal.household_id},
    ).first()
    if ok is None:
        raise HTTPException(404, detail={"code": "unknown", "message": "No such member in this household."})


def _is_owner_grant(conn: Connection, user_id: UUID, profile_id: UUID) -> bool:
    return (
        conn.execute(
            text("select role from profile_grant where user_id = :u and profile_id = :p"),
            {"u": user_id, "p": profile_id},
        ).scalar()
        == "owner"
    )


@router.put("/grants")
def put_grant(body: GrantIn, conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    sessions.require_step_up(conn, principal)
    perm = _manage(conn, principal, body.profile)
    _target_user(conn, principal, body.user_id)
    if _is_owner_grant(conn, body.user_id, perm.profile.id):
        raise HTTPException(409, detail={"code": "owner_grant", "message": "The owner's own access cannot change."})
    cats = set(body.categories) if body.categories is not None else None
    if cats is not None and cats - ALL_SCOPES:
        raise HTTPException(
            422, detail={"code": "bad_input", "message": f"Unknown categories {sorted(cats - ALL_SCOPES)}"}
        )
    set_grant(
        conn, body.user_id, perm.profile.id, body.role, cats, granted_by=principal.user_id, expires_at=body.expires_at
    )
    _audit(
        conn,
        principal,
        "grant_change",
        "PUT /api/grants",
        perm.profile.id,
        resource_type="user",
        resource_ids=[str(body.user_id)],
        args_summary=f"role={body.role};categories={','.join(sorted(cats)) if cats is not None else 'default'}",
    )
    return {"ok": True}


@router.delete("/grants")
def delete_grant(
    user_id: UUID, profile: str, conn: Connection = Depends(get_conn), principal: Principal = SessionOnly
) -> dict:
    sessions.require_step_up(conn, principal)
    perm = _manage(conn, principal, profile)
    if _is_owner_grant(conn, user_id, perm.profile.id):
        raise HTTPException(409, detail={"code": "owner_grant", "message": "The owner's own access cannot change."})
    if not revoke_grant(conn, user_id, perm.profile.id):
        raise HTTPException(404, detail={"code": "unknown", "message": "No such grant."})
    _audit(
        conn,
        principal,
        "grant_change",
        "DELETE /api/grants",
        perm.profile.id,
        resource_type="user",
        resource_ids=[str(user_id)],
        args_summary="revoke",
    )
    return {"ok": True}


# ------------------------------------------------------ locks, sensitive switches ----


class LocksIn(BaseModel):
    locks: list[str]


@router.put("/profiles/{p}/locks")
def put_locks(p: str, body: LocksIn, conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    """Categories switched off for everyone but the owner. Removing a lock needs step-up."""
    perm = _manage(conn, principal, p)
    new = set(body.locks)
    if new - ALL_SCOPES:
        raise HTTPException(422, detail={"code": "bad_input", "message": f"Unknown scopes {sorted(new - ALL_SCOPES)}"})
    if perm.profile.locks - new:
        sessions.require_step_up(conn, principal)
    set_locks(conn, perm.profile.id, new)
    _audit(
        conn, principal, "lock_change", "PUT /api/profiles/locks", perm.profile.id, args_summary=",".join(sorted(new))
    )
    return {"ok": True, "locks": sorted(new)}


class SensitiveIn(BaseModel):
    ui: bool | None = None
    agents: bool | None = None


@router.put("/profiles/{p}/sensitive")
def put_sensitive(
    p: str,
    body: SensitiveIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    settings: Settings = Depends(get_settings),
) -> dict:
    """The two consents for sensitive genome findings: show them in the app, share them with agents."""
    perm = _manage(conn, principal, p)
    turning_on = (body.ui and not perm.profile.genome_sensitive_ui) or (
        body.agents and not perm.profile.genome_sensitive_agents
    )
    if turning_on:
        if perm.profile.is_minor(date.today(), settings.age_of_majority):
            raise HTTPException(
                409, detail={"code": "minor", "message": "Sensitive findings stay withheld until the age of majority."}
            )
        sessions.require_step_up(conn, principal)
    set_genome_switches(conn, perm.profile.id, ui=body.ui, agents=body.agents)
    _audit(
        conn,
        principal,
        "consent_change",
        "PUT /api/profiles/sensitive",
        perm.profile.id,
        args_summary=f"ui={body.ui};agents={body.agents}",
    )
    after = effective_permission(conn, principal, perm.profile.id).profile
    return {"ok": True, "ui": after.genome_sensitive_ui, "agents": after.genome_sensitive_agents}


# ------------------------------------------------------------------- deletion ----


def _delete_profile_now(rt: Runtime, principal: Principal, profile_id: UUID, slug: str) -> dict:
    """Own transaction, so the blob files go only after the rows are gone for good."""
    with rt.engine.begin() as c:
        result = household.delete_profile(c, profile_id)
        _audit(
            c,
            principal,
            "purge",
            "DELETE /api/profiles",
            profile_id,
            resource_type="profile",
            resource_ids=[str(profile_id)],
            rows_out=sum(result["rows"].values()),
            args_summary=f"blobs={len(result['blobs'])}",
        )
    for sha in result["blobs"]:
        rt.blobs.delete(sha)
    return {"deleted": slug, "rows": result["rows"], "blobs": len(result["blobs"])}


@router.delete("/profiles/{p}")
def delete_profile(
    p: str,
    confirm: str,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    rt: Runtime = Depends(get_runtime),
) -> dict:
    """Hard delete of the profile's rows and files. `confirm` must repeat the slug. Older backups still
    hold the data until they expire."""
    sessions.require_step_up(conn, principal)
    perm = _manage(conn, principal, p)
    if confirm != perm.profile.slug:
        raise HTTPException(422, detail={"code": "confirm_mismatch", "message": "Type the profile's short name."})
    conn.commit()  # release this request's transaction before the deletion's own
    try:
        return _delete_profile_now(rt, principal, perm.profile.id, perm.profile.slug)
    except household.HouseholdError as exc:
        raise _fail(exc) from exc


@router.delete("/account")
def delete_my_account(
    confirm: str,
    response: Response,
    request: Request,
    delete_profile: bool = False,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    rt: Runtime = Depends(get_runtime),
) -> dict:
    """Close my login; with `delete_profile=true` also hard-delete my own profile. `confirm` repeats my name."""
    sessions.require_step_up(conn, principal)
    name = conn.execute(text("select display_name from app_user where id = :u"), {"u": principal.user_id}).scalar()
    if confirm.strip() != name:
        raise HTTPException(422, detail={"code": "confirm_mismatch", "message": "Type your name as shown."})
    own = conn.execute(text("select id, slug from profile where user_id = :u"), {"u": principal.user_id}).first()
    if delete_profile and own is not None:
        _manage(conn, principal, own.slug)
    stranded = household.orphaned_profiles(
        conn, principal.user_id, except_profile=own.id if delete_profile and own is not None else None
    )
    if stranded:
        raise HTTPException(
            409,
            detail={
                "code": "last_manager",
                "message": "Nobody else can reach these profiles. Delete them or grant someone access first.",
                "profiles": stranded,
            },
        )
    conn.commit()
    deleted = None
    if delete_profile and own is not None:
        deleted = _delete_profile_now(rt, principal, own.id, own.slug)
    try:
        with rt.engine.begin() as c:
            household.delete_account(c, principal.user_id)
            _audit(
                c,
                principal,
                "account_delete",
                "DELETE /api/account",
                None,
                resource_type="user",
                resource_ids=[str(principal.user_id)],
            )
    except household.HouseholdError as exc:
        raise _fail(exc) from exc
    response.delete_cookie(sessions.COOKIE, path="/")
    return {"ok": True, "profile_deleted": deleted}
