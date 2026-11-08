"""Passkey login with py_webauthn (security-privacy.md 5).

WebAuthn runs only in a secure context: https, or http on localhost. The relying party id is the host
of OWNHEALTH_BASE_URL and the expected origin is that URL, so a deploy behind the HTTPS reverse proxy
works; a plain-HTTP LAN address (http://192.168.1.20:8080) cannot run the ceremony at all, which is
the one case the password fallback exists for (`auth.password`).

Every challenge is single use and expires after 5 minutes (`auth_challenge`). Login is username-less:
the passkey is a discoverable credential and the browser picks it.
"""

from __future__ import annotations

import base64
import json
from dataclasses import dataclass
from uuid import UUID

import webauthn
from sqlalchemy import Connection, text
from webauthn.helpers.exceptions import InvalidAuthenticationResponse, InvalidRegistrationResponse
from webauthn.helpers.structs import (
    AuthenticatorSelectionCriteria,
    AuthenticatorTransport,
    PublicKeyCredentialDescriptor,
    ResidentKeyRequirement,
    UserVerificationRequirement,
)

from ownhealth.auth import challenges

RP_NAME = "OwnHealth"


class PasskeyError(ValueError):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class Registered:
    challenge: challenges.Challenge
    credential_id: bytes
    public_key: bytes
    sign_count: int
    transports: list[str]


def _b64url_decode(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def _client_challenge(credential: dict) -> bytes:
    try:
        client_data = json.loads(_b64url_decode(credential["response"]["clientDataJSON"]))
        return _b64url_decode(client_data["challenge"])
    except (KeyError, TypeError, ValueError) as exc:
        raise PasskeyError("passkey_invalid", "The passkey response is malformed.") from exc


def _options(opts) -> dict:
    return json.loads(webauthn.options_to_json(opts))


def _user_credentials(conn: Connection, user_id: UUID) -> list[PublicKeyCredentialDescriptor]:
    rows = conn.execute(text("select id, transports from passkey where user_id = :u"), {"u": user_id}).all()
    out = []
    for r in rows:
        transports = [AuthenticatorTransport(t) for t in r.transports if t in AuthenticatorTransport._value2member_map_]
        out.append(PublicKeyCredentialDescriptor(id=bytes(r.id), transports=transports or None))
    return out


# ------------------------------------------------------------- registration ----


def begin_registration(
    conn: Connection,
    *,
    user_id: UUID,
    rp_id: str,
    origin: str,
    user_name: str = "",
    purpose: str = "register",
    data: dict | None = None,
) -> dict:
    """Creation options for navigator.credentials.create(). `user_id` may be a not-yet-created user
    (first-run setup, invite): then pass purpose 'setup' or 'enroll' and the row is checked at finish."""
    existing = purpose == "register"
    opts = webauthn.generate_registration_options(
        rp_id=rp_id,
        rp_name=RP_NAME,
        user_id=user_id.bytes,
        user_name=user_name or str(user_id),
        user_display_name=user_name or None,
        authenticator_selection=AuthenticatorSelectionCriteria(
            resident_key=ResidentKeyRequirement.REQUIRED,
            user_verification=UserVerificationRequirement.REQUIRED,
        ),
        exclude_credentials=_user_credentials(conn, user_id) if existing else None,
    )
    challenges.issue(
        conn,
        purpose,
        opts.challenge,
        user_id=user_id if existing else None,
        data={**(data or {}), "user_id": str(user_id), "origin": origin},
    )
    return _options(opts)


def verify_registration(
    conn: Connection, *, credential: dict, rp_id: str, origin: str, purposes: tuple[str, ...] = ("register",)
) -> Registered:
    """Check the attestation against a live challenge; consumes it. Does not store anything."""
    ch = challenges.consume(conn, _client_challenge(credential), purposes)
    try:
        v = webauthn.verify_registration_response(
            credential=credential,
            expected_challenge=ch.challenge,
            expected_rp_id=rp_id,
            expected_origin=origin,
            require_user_verification=True,
        )
    except InvalidRegistrationResponse as exc:
        raise PasskeyError("passkey_invalid", f"The passkey could not be verified: {exc}") from exc
    transports = [str(t) for t in (credential.get("response", {}).get("transports") or []) if isinstance(t, str)]
    return Registered(ch, v.credential_id, v.credential_public_key, v.sign_count, transports[:8])


def store_passkey(conn: Connection, user_id: UUID, reg: Registered, label: str | None = None) -> bytes:
    exists = conn.execute(text("select 1 from passkey where id = :id"), {"id": reg.credential_id}).first()
    if exists:
        raise PasskeyError("passkey_exists", "This passkey is already registered.")
    conn.execute(
        text(
            "insert into passkey (id, user_id, public_key, sign_count, transports, label)"
            " values (:id, :u, :pk, :sc, :tr, :l)"
        ),
        {
            "id": reg.credential_id,
            "u": user_id,
            "pk": reg.public_key,
            "sc": reg.sign_count,
            "tr": reg.transports,
            "l": (label or "").strip()[:80] or None,
        },
    )
    return reg.credential_id


def finish_registration(
    conn: Connection, *, user_id: UUID, credential: dict, rp_id: str, origin: str, label: str | None = None
) -> UUID:
    """Add a passkey to an existing, logged-in user. Returns the user id."""
    reg = verify_registration(conn, credential=credential, rp_id=rp_id, origin=origin, purposes=("register",))
    if reg.challenge.user_id != user_id:
        raise PasskeyError("passkey_invalid", "This registration was started by another user.")
    store_passkey(conn, user_id, reg, label)
    return user_id


# -------------------------------------------------------------------- login ----


def begin_login(conn: Connection, *, rp_id: str, user_id: UUID | None = None, session_id: UUID | None = None) -> dict:
    """Request options for navigator.credentials.get(). With `user_id` it is a step-up for that user."""
    allow = _user_credentials(conn, user_id) if user_id else []
    if user_id and not allow:
        raise PasskeyError("no_passkey", "This account has no passkey yet.")
    opts = webauthn.generate_authentication_options(
        rp_id=rp_id, allow_credentials=allow, user_verification=UserVerificationRequirement.REQUIRED
    )
    challenges.issue(conn, "step_up" if user_id else "login", opts.challenge, user_id=user_id, session_id=session_id)
    return _options(opts)


def finish_login(conn: Connection, *, credential: dict, rp_id: str, origin: str, purpose: str = "login") -> UUID:
    """Verify an assertion. Returns the user id; the caller creates the session with
    `auth.sessions.create_session` (login) or marks it stepped up (step_up)."""
    ch = challenges.consume(conn, _client_challenge(credential), (purpose,))
    try:
        cred_id = _b64url_decode(credential.get("rawId") or credential["id"])
    except (KeyError, TypeError, ValueError) as exc:
        raise PasskeyError("passkey_invalid", "The passkey response is malformed.") from exc
    row = conn.execute(
        text(
            "select p.user_id, p.public_key, p.sign_count from passkey p join app_user u on u.id = p.user_id"
            " where p.id = :id and u.disabled_at is null"
        ),
        {"id": cred_id},
    ).first()
    if row is None:
        raise PasskeyError("passkey_unknown", "This passkey is not registered here.")
    if ch.user_id is not None and ch.user_id != row.user_id:
        raise PasskeyError("passkey_invalid", "This passkey belongs to another user.")
    try:
        v = webauthn.verify_authentication_response(
            credential=credential,
            expected_challenge=ch.challenge,
            expected_rp_id=rp_id,
            expected_origin=origin,
            credential_public_key=bytes(row.public_key),
            credential_current_sign_count=row.sign_count,
            require_user_verification=True,
        )
    except InvalidAuthenticationResponse as exc:
        raise PasskeyError("passkey_invalid", f"The passkey could not be verified: {exc}") from exc
    conn.execute(
        text("update passkey set sign_count = :sc, last_used_at = now() where id = :id"),
        {"sc": v.new_sign_count, "id": cred_id},
    )
    return row.user_id


def list_passkeys(conn: Connection, user_id: UUID) -> list[dict]:
    rows = conn.execute(
        text(
            "select id, label, transports, created_at, last_used_at from passkey where user_id = :u order by created_at"
        ),
        {"u": user_id},
    ).mappings()
    return [{**dict(r), "id": base64.urlsafe_b64encode(bytes(r["id"])).rstrip(b"=").decode()} for r in rows]


def delete_passkey(conn: Connection, user_id: UUID, credential_id_b64: str) -> bool:
    res = conn.execute(
        text("delete from passkey where id = :id and user_id = :u"),
        {"id": _b64url_decode(credential_id_b64), "u": user_id},
    )
    return res.rowcount > 0
