"""Login, first-run setup, invites, step-up, passkeys, recovery codes, OIDC (security-privacy.md 5).

The browser runs the WebAuthn ceremonies: `.../begin` returns the options JSON for
`navigator.credentials.create()` / `.get()`, `.../finish` takes the credential JSON back
(`PublicKeyCredential.toJSON()`). A successful login sets the `oh_session` cookie.
"""

from __future__ import annotations

import time
from datetime import date
from urllib.parse import quote, urlsplit
from uuid import UUID, uuid4

import httpx
from fastapi import APIRouter, Depends, HTTPException, Request, Response
from fastapi.responses import RedirectResponse
from pydantic import BaseModel, Field
from sqlalchemy import Connection, text

from ownhealth.api._deps import get_conn, get_settings
from ownhealth.auth import challenges, csrf, household, oidc, passkeys, password, recovery, sessions
from ownhealth.auth import principal as principal_mod
from ownhealth.auth.deps import SessionOnly, StepUp, client_ip
from ownhealth.config import Settings
from ownhealth.core.access import Principal
from ownhealth.core.audit import AuditEvent, write_audit

router = APIRouter(prefix="/auth", tags=["auth"])

_ERRORS = (passkeys.PasskeyError, challenges.ChallengeError, household.HouseholdError, oidc.OIDCError)


def _fail(exc: Exception, status: int = 400) -> HTTPException:
    code = getattr(exc, "code", "bad_input")
    if code in ("already_set_up", "already_claimed", "passkey_exists", "last_manager"):
        status = 409
    extra = getattr(exc, "extra", {}) or {}
    return HTTPException(status, detail={"code": code, "message": str(exc), **extra})


def rp(settings: Settings) -> tuple[str, str]:
    """(rp_id, expected origin) from OWNHEALTH_BASE_URL."""
    parts = urlsplit(settings.base_url)
    return (parts.hostname or "localhost"), f"{parts.scheme}://{parts.netloc}"


def same_origin(request: Request, settings: Settings = Depends(get_settings)) -> None:
    csrf.check_origin(request, settings.base_url)


Public = Depends(same_origin)


def _set_cookie(response: Response, secret: str, settings: Settings) -> None:
    response.set_cookie(
        sessions.COOKIE,
        secret,
        max_age=int(sessions.ABSOLUTE.total_seconds()),
        httponly=True,
        secure=settings.base_url.startswith("https://"),
        samesite="lax",
        path="/",
    )


def _login(
    conn: Connection, request: Request, response: Response, settings: Settings, user_id: UUID, method: str
) -> str:
    ip = client_ip(request)
    secret = sessions.create_session(conn, user_id, ip=ip, user_agent=request.headers.get("user-agent"), method=method)
    write_audit(
        conn,
        AuditEvent(
            actor_kind="user",
            actor_id=user_id,
            via="ui",
            action="login",
            tool=f"auth:{method}",
            ip=ip,
            user_agent=(request.headers.get("user-agent") or "")[:300] or None,
        ),
    )
    _set_cookie(response, secret, settings)
    return secret


def _require_passkeys(settings: Settings) -> None:
    if not password.passkeys_possible(settings.base_url):
        raise HTTPException(
            400,
            detail={
                "code": "passkeys_unavailable",
                "message": "Passkeys need https (or localhost). Use the HTTPS address, or the password fallback.",
            },
        )


def _require_no_passkeys(settings: Settings) -> None:
    if password.passkeys_possible(settings.base_url):
        raise HTTPException(
            403, detail={"code": "passkeys_available", "message": "This address supports passkeys: use one."}
        )


def _oidc_config(request: Request) -> oidc.OIDCConfig | None:
    if hasattr(request.app.state, "oidc_config"):  # tests inject a fake provider
        return request.app.state.oidc_config
    return oidc.OIDCConfig.from_env()


def _oidc_http(request: Request) -> httpx.Client:
    return getattr(request.app.state, "oidc_http", None) or httpx.Client()


def _user(conn: Connection, user_id: UUID) -> dict:
    row = (
        conn.execute(text("select id, display_name, locale, household_id from app_user where id = :u"), {"u": user_id})
        .mappings()
        .one()
    )
    return dict(row)


# ------------------------------------------------------------------- state ----


@router.get("/state")
def state(request: Request, conn: Connection = Depends(get_conn), settings: Settings = Depends(get_settings)) -> dict:
    """What the login page needs: first run or not, which methods work here, who is logged in."""
    p = principal_mod.resolve(
        conn,
        settings,
        authorization=None,
        session_secret=request.cookies.get(sessions.COOKIE),
    )
    cfg = _oidc_config(request)
    user, fresh = None, False
    if p is not None and p.user_id is not None:
        user = _user(conn, p.user_id)
        user = {
            "id": user["id"],
            "display_name": user["display_name"],
            "locale": user["locale"],
            "passkeys": conn.execute(
                text("select count(*) from passkey where user_id = :u"), {"u": p.user_id}
            ).scalar(),
            "recovery_codes_left": recovery.remaining(conn, p.user_id),
            "oidc_linked": conn.execute(
                text("select oidc_subject is not null from app_user where id = :u"), {"u": p.user_id}
            ).scalar(),
        }
        try:
            sessions.require_step_up(conn, p)
            fresh = True
        except Exception:  # noqa: BLE001
            fresh = False
    possible = password.passkeys_possible(settings.base_url)
    return {
        "setup_needed": not household.users_exist(conn),
        "passkeys_possible": possible,
        "password_allowed": not possible,
        "oidc": {"enabled": cfg is not None, "label": cfg.label if cfg else None},
        "user": user,
        "method": p.extra.get("method") if p is not None else None,
        "auth_kind": p.kind if p is not None else None,
        "step_up_fresh": fresh,
        "step_up_seconds": sessions.STEP_UP_SECONDS,
    }


# ---------------------------------------------------------------- first run ----


class SetupIn(BaseModel):
    household_name: str = Field("", max_length=80)
    display_name: str = Field(min_length=1, max_length=80)
    locale: str = "es"
    birth_date: date | None = None
    sex_at_birth: str | None = None
    country: str | None = None


class CredentialIn(BaseModel):
    credential: dict
    label: str | None = Field(None, max_length=80)


@router.post("/setup/begin", dependencies=[Public])
def setup_begin(body: SetupIn, conn: Connection = Depends(get_conn), settings: Settings = Depends(get_settings)):
    _require_passkeys(settings)
    if household.users_exist(conn):
        raise HTTPException(409, detail={"code": "already_set_up", "message": "This instance already has an owner."})
    rp_id, origin = rp(settings)
    return passkeys.begin_registration(
        conn,
        user_id=uuid4(),
        rp_id=rp_id,
        origin=origin,
        user_name=body.display_name,
        purpose="setup",
        data={"setup": body.model_dump(mode="json")},
    )


@router.post("/setup/finish", dependencies=[Public])
def setup_finish(
    body: CredentialIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    rp_id, origin = rp(settings)
    try:
        reg = passkeys.verify_registration(
            conn, credential=body.credential, rp_id=rp_id, origin=origin, purposes=("setup",)
        )
        s = SetupIn(**reg.challenge.data["setup"])
        owner = household.setup_owner(
            conn,
            user_id=UUID(reg.challenge.data["user_id"]),
            household_name=s.household_name,
            display_name=s.display_name,
            locale=s.locale,
            birth_date=s.birth_date,
            sex_at_birth=s.sex_at_birth,
            country=s.country,
        )
        passkeys.store_passkey(conn, owner.user_id, reg, body.label)
    except _ERRORS as exc:
        raise _fail(exc) from exc
    codes = recovery.new_codes(conn, owner.user_id)
    _login(conn, request, response, settings, owner.user_id, "passkey")
    return {"user_id": owner.user_id, "profile": owner.slug, "recovery_codes": codes}


class PasswordSetupIn(SetupIn):
    password: str = Field(min_length=password.MIN_LENGTH, max_length=256)


@router.post("/setup/password", dependencies=[Public])
def setup_password(
    body: PasswordSetupIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    """First run on a plain-HTTP LAN address, where passkeys cannot run."""
    _require_no_passkeys(settings)
    try:
        owner = household.setup_owner(
            conn,
            household_name=body.household_name,
            display_name=body.display_name,
            locale=body.locale,
            birth_date=body.birth_date,
            sex_at_birth=body.sex_at_birth,
            country=body.country,
        )
    except _ERRORS as exc:
        raise _fail(exc) from exc
    password.set_password(conn, owner.user_id, body.password)
    codes = recovery.new_codes(conn, owner.user_id)
    _login(conn, request, response, settings, owner.user_id, "password")
    return {"user_id": owner.user_id, "profile": owner.slug, "recovery_codes": codes}


# -------------------------------------------------------------------- login ----


@router.post("/login/begin", dependencies=[Public])
def login_begin(conn: Connection = Depends(get_conn), settings: Settings = Depends(get_settings)):
    _require_passkeys(settings)
    return passkeys.begin_login(conn, rp_id=rp(settings)[0])


@router.post("/login/finish", dependencies=[Public])
def login_finish(
    body: CredentialIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    rp_id, origin = rp(settings)
    try:
        user_id = passkeys.finish_login(conn, credential=body.credential, rp_id=rp_id, origin=origin)
    except _ERRORS as exc:
        raise _fail(exc, 401) from exc
    _login(conn, request, response, settings, user_id, "passkey")
    return {"user_id": user_id}


class RecoveryIn(BaseModel):
    code: str = Field(min_length=1, max_length=40)


@router.post("/recovery", dependencies=[Public])
def recovery_login(
    body: RecoveryIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    """Log in with one recovery code. It is used up; the session has no step-up until a new login."""
    user_id = recovery.use_code(conn, body.code)
    if user_id is None:
        raise HTTPException(401, detail={"code": "recovery_invalid", "message": "Unknown or used recovery code."})
    _login(conn, request, response, settings, user_id, "recovery")
    return {"user_id": user_id, "recovery_codes_left": recovery.remaining(conn, user_id)}


class PasswordLoginIn(BaseModel):
    name: str = Field(min_length=1, max_length=80)
    password: str = Field(min_length=1, max_length=256)


@router.post("/password/login", dependencies=[Public])
def password_login(
    body: PasswordLoginIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    _require_no_passkeys(settings)
    rows = conn.execute(
        text(
            "select id, password_hash from app_user where lower(display_name) = lower(:n)"
            " and password_hash is not null and disabled_at is null"
        ),
        {"n": body.name.strip()},
    ).all()
    ok = len(rows) == 1 and password.check_password(body.password, rows[0].password_hash)
    if not rows:
        password.check_password(body.password, password.hash_password("timing"))  # same cost on a miss
    if not ok:
        raise HTTPException(401, detail={"code": "login_failed", "message": "Wrong name or password."})
    _login(conn, request, response, settings, rows[0].id, "password")
    return {"user_id": rows[0].id}


@router.post("/logout", dependencies=[Public])
def logout(request: Request, response: Response, conn: Connection = Depends(get_conn)) -> dict:
    secret = request.cookies.get(sessions.COOKIE)
    if secret:
        sessions.revoke_session(conn, secret)
    response.delete_cookie(sessions.COOKIE, path="/")
    return {"ok": True}


# ------------------------------------------------------------------ step-up ----


class StepUpIn(BaseModel):
    credential: dict | None = None
    password: str | None = Field(None, max_length=256)


@router.post("/step-up/begin")
def step_up_begin(
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    settings: Settings = Depends(get_settings),
) -> dict:
    """Which ceremony confirms it is you: a passkey (options returned), the password, or OIDC."""
    sid = sessions.session_id(principal)
    has_passkey = conn.execute(
        text("select exists (select 1 from passkey where user_id = :u)"), {"u": principal.user_id}
    ).scalar()
    if has_passkey and password.passkeys_possible(settings.base_url):
        options = passkeys.begin_login(conn, rp_id=rp(settings)[0], user_id=principal.user_id, session_id=sid)
        return {"method": "passkey", "options": options}
    if conn.execute(
        text("select password_hash is not null from app_user where id = :u"), {"u": principal.user_id}
    ).scalar():
        return {"method": "password"}
    if conn.execute(
        text("select oidc_subject is not null from app_user where id = :u"), {"u": principal.user_id}
    ).scalar():
        return {"method": "oidc", "url": "/api/auth/oidc/login?mode=step_up"}
    raise HTTPException(409, detail={"code": "no_method", "message": "Add a passkey first."})


@router.post("/step-up/finish")
def step_up_finish(
    body: StepUpIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    settings: Settings = Depends(get_settings),
) -> dict:
    sid = sessions.session_id(principal)
    if sid is None:
        raise HTTPException(403, detail={"code": "step_up_required", "message": "Log in in the browser first."})
    if body.credential is not None:
        rp_id, origin = rp(settings)
        try:
            user_id = passkeys.finish_login(
                conn, credential=body.credential, rp_id=rp_id, origin=origin, purpose="step_up"
            )
        except _ERRORS as exc:
            raise _fail(exc, 401) from exc
        if user_id != principal.user_id:  # a challenge another user started, answered with their passkey
            raise HTTPException(401, detail={"code": "passkey_invalid", "message": "Another user's passkey."})
    elif body.password is not None:
        if (
            password.passkeys_possible(settings.base_url)
            and conn.execute(
                text("select exists (select 1 from passkey where user_id = :u)"), {"u": principal.user_id}
            ).scalar()
        ):
            raise HTTPException(403, detail={"code": "passkeys_available", "message": "Use your passkey."})
        if not password.verify_user_password(conn, principal.user_id, body.password):
            raise HTTPException(401, detail={"code": "login_failed", "message": "Wrong password."})
    else:
        raise HTTPException(422, detail={"code": "bad_input", "message": "Send a passkey credential or the password."})
    sessions.mark_step_up(conn, sid)
    return {"ok": True, "valid_seconds": sessions.STEP_UP_SECONDS}


# ---------------------------------------------------- passkeys, recovery codes ----


@router.get("/passkeys")
def list_my_passkeys(conn: Connection = Depends(get_conn), principal: Principal = SessionOnly) -> dict:
    return {"items": passkeys.list_passkeys(conn, principal.user_id)}


@router.post("/passkeys/begin")
def add_passkey_begin(
    conn: Connection = Depends(get_conn), principal: Principal = SessionOnly, settings: Settings = Depends(get_settings)
) -> dict:
    """Add a passkey (a second device, or a new one after a recovery-code login)."""
    _require_passkeys(settings)
    rp_id, origin = rp(settings)
    name = _user(conn, principal.user_id)["display_name"]
    return passkeys.begin_registration(conn, user_id=principal.user_id, rp_id=rp_id, origin=origin, user_name=name)


@router.post("/passkeys/finish")
def add_passkey_finish(
    body: CredentialIn,
    conn: Connection = Depends(get_conn),
    principal: Principal = SessionOnly,
    settings: Settings = Depends(get_settings),
) -> dict:
    rp_id, origin = rp(settings)
    try:
        passkeys.finish_registration(
            conn, user_id=principal.user_id, credential=body.credential, rp_id=rp_id, origin=origin, label=body.label
        )
    except _ERRORS as exc:
        raise _fail(exc) from exc
    return {"items": passkeys.list_passkeys(conn, principal.user_id)}


@router.delete("/passkeys/{credential_id}")
def remove_passkey(credential_id: str, conn: Connection = Depends(get_conn), principal: Principal = StepUp) -> dict:
    other_way_in = conn.execute(
        text(
            "select (select count(*) from passkey where user_id = :u) > 1 or password_hash is not null"
            " or oidc_subject is not null from app_user where id = :u"
        ),
        {"u": principal.user_id},
    ).scalar()
    if not other_way_in:
        raise HTTPException(
            409, detail={"code": "last_passkey", "message": "This is your only way in. Add another first."}
        )
    if not passkeys.delete_passkey(conn, principal.user_id, credential_id):
        raise HTTPException(404, detail={"code": "unknown", "message": "Unknown passkey."})
    return {"ok": True}


@router.post("/recovery-codes")
def regenerate_recovery_codes(conn: Connection = Depends(get_conn), principal: Principal = StepUp) -> dict:
    """Ten new codes; the old ones stop working. Shown once."""
    return {"recovery_codes": recovery.new_codes(conn, principal.user_id)}


# ------------------------------------------------------------------- invites ----


@router.get("/invite/{code}")
def invite_info(
    code: str, request: Request, conn: Connection = Depends(get_conn), settings: Settings = Depends(get_settings)
) -> dict:
    ch = household.peek_invite(conn, code)
    if ch is None:
        raise HTTPException(404, detail={"code": "invite_invalid", "message": "This invite expired or was used."})
    row = conn.execute(
        text("select u.display_name, h.name from app_user u join household h on h.id = u.household_id where u.id = :u"),
        {"u": ch.user_id},
    ).one()
    possible = password.passkeys_possible(settings.base_url)
    return {
        "display_name": row.display_name,
        "household": row.name,
        "claims_profile": ch.data.get("claim_profile_id") is not None,
        "passkeys_possible": possible,
        "password_allowed": not possible,
        "oidc": _oidc_config(request) is not None,
    }


class EnrollIn(BaseModel):
    code: str = Field(min_length=10, max_length=80)


class EnrollFinishIn(CredentialIn):
    code: str = Field(min_length=10, max_length=80)


@router.post("/enroll/begin", dependencies=[Public])
def enroll_begin(body: EnrollIn, conn: Connection = Depends(get_conn), settings: Settings = Depends(get_settings)):
    _require_passkeys(settings)
    ch = household.peek_invite(conn, body.code)
    if ch is None:
        raise HTTPException(400, detail={"code": "invite_invalid", "message": "This invite expired or was used."})
    rp_id, origin = rp(settings)
    return passkeys.begin_registration(
        conn,
        user_id=ch.user_id,
        rp_id=rp_id,
        origin=origin,
        user_name=_user(conn, ch.user_id)["display_name"],
        purpose="enroll",
    )


def _enrolled(conn, request, response, settings, user_id: UUID, method: str) -> dict:
    codes = recovery.new_codes(conn, user_id)
    _login(conn, request, response, settings, user_id, method)
    return {"user_id": user_id, "recovery_codes": codes}


@router.post("/enroll/finish", dependencies=[Public])
def enroll_finish(
    body: EnrollFinishIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    rp_id, origin = rp(settings)
    try:
        reg = passkeys.verify_registration(
            conn, credential=body.credential, rp_id=rp_id, origin=origin, purposes=("enroll",)
        )
        user_id = household.redeem_invite(conn, body.code)
        if str(user_id) != reg.challenge.data.get("user_id"):
            raise household.HouseholdError("invite_invalid", "This passkey was made for another invite.")
        passkeys.store_passkey(conn, user_id, reg, body.label)
    except _ERRORS as exc:
        raise _fail(exc) from exc
    return _enrolled(conn, request, response, settings, user_id, "passkey")


class EnrollPasswordIn(EnrollIn):
    password: str = Field(min_length=password.MIN_LENGTH, max_length=256)


@router.post("/enroll/password", dependencies=[Public])
def enroll_password(
    body: EnrollPasswordIn,
    request: Request,
    response: Response,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    _require_no_passkeys(settings)
    try:
        user_id = household.redeem_invite(conn, body.code)
    except _ERRORS as exc:
        raise _fail(exc) from exc
    name = _user(conn, user_id)["display_name"]
    taken = conn.execute(
        text(
            "select exists (select 1 from app_user where lower(display_name) = lower(:n) and id <> :u"
            " and password_hash is not null)"
        ),
        {"n": name, "u": user_id},
    ).scalar()
    if taken:
        raise HTTPException(409, detail={"code": "name_taken", "message": "Another login uses this name."})
    password.set_password(conn, user_id, body.password)
    return _enrolled(conn, request, response, settings, user_id, "password")


# ---------------------------------------------------------------------- OIDC ----


def _redirect_uri(settings: Settings) -> str:
    return f"{settings.base_url}/api/auth/oidc/callback"


@router.get("/oidc/login")
def oidc_login(
    request: Request,
    mode: str = "login",
    invite: str | None = None,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    """Redirect to the provider. mode: login | link (logged in) | enroll (with ?invite=) | step_up."""
    cfg = _oidc_config(request)
    if cfg is None:
        raise HTTPException(404, detail={"code": "oidc_disabled", "message": "OIDC is not configured."})
    user_id = session_id = None
    data: dict = {}
    if mode in ("link", "step_up"):
        p = principal_mod.resolve(
            conn, settings, authorization=None, session_secret=request.cookies.get(sessions.COOKIE)
        )
        if p is None or p.kind != "session":
            raise HTTPException(401, detail={"code": "unauthenticated", "message": "Log in first."})
        user_id, session_id = p.user_id, sessions.session_id(p)
    elif mode == "enroll":
        ch = household.peek_invite(conn, invite or "")
        if ch is None:
            raise HTTPException(400, detail={"code": "invite_invalid", "message": "This invite expired or was used."})
        data["invite"] = invite
    try:
        url = oidc.begin(
            conn,
            cfg,
            _oidc_http(request),
            mode=mode,
            redirect_uri=_redirect_uri(settings),
            user_id=user_id,
            session_id=session_id,
            data=data,
        )
    except _ERRORS as exc:
        raise _fail(exc) from exc
    return RedirectResponse(url, status_code=302)


def _back(path: str = "/", error: str | None = None) -> RedirectResponse:
    return RedirectResponse(f"{path}?auth_error={quote(error)}" if error else path, status_code=302)


@router.get("/oidc/callback")
def oidc_callback(
    request: Request,
    state: str = "",
    code: str = "",
    error: str | None = None,
    conn: Connection = Depends(get_conn),
    settings: Settings = Depends(get_settings),
):
    cfg = _oidc_config(request)
    if cfg is None:
        raise HTTPException(404, detail={"code": "oidc_disabled", "message": "OIDC is not configured."})
    if error or not state or not code:
        return _back("/login", "oidc_" + (error or "failed")[:40])
    try:
        res = oidc.finish(conn, cfg, _oidc_http(request), state=state, code=code, redirect_uri=_redirect_uri(settings))
    except _ERRORS as exc:
        return _back("/login", getattr(exc, "code", "oidc_failed"))
    owner = conn.execute(
        text("select id from app_user where oidc_subject = :s and disabled_at is null"), {"s": res.subject}
    ).scalar()
    resp = _back("/")
    if res.mode == "login":
        if owner is None and not household.users_exist(conn):
            owner = household.setup_owner(
                conn, household_name="", display_name=res.name or "Owner", oidc_subject=res.subject
            ).user_id
        if owner is None:
            return _back("/login", "oidc_not_linked")
        _login(conn, request, resp, settings, owner, "oidc")
        return resp
    if res.mode == "enroll":
        if owner is not None:
            return _back("/login", "oidc_already_linked")
        try:
            user_id = household.redeem_invite(conn, res.challenge.data.get("invite") or "")
        except _ERRORS as exc:
            return _back("/login", exc.code)
        conn.execute(text("update app_user set oidc_subject = :s where id = :u"), {"s": res.subject, "u": user_id})
        _login(conn, request, resp, settings, user_id, "oidc")
        return resp
    if res.mode == "link":
        if owner is not None and owner != res.challenge.user_id:
            return _back("/settings", "oidc_already_linked")
        conn.execute(
            text("update app_user set oidc_subject = :s where id = :u"), {"s": res.subject, "u": res.challenge.user_id}
        )
        return _back("/settings")
    # step_up: the same person at the provider, and a login the provider just asked for
    if owner is None or owner != res.challenge.user_id or res.challenge.session_id is None:
        return _back("/", "oidc_wrong_user")
    if res.auth_time is not None and time.time() - res.auth_time > sessions.STEP_UP_SECONDS:
        return _back("/", "oidc_not_fresh")
    sessions.mark_step_up(conn, res.challenge.session_id)
    return resp
