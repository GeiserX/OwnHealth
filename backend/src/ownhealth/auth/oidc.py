"""Optional OIDC login against any OpenID Connect provider (Pocket ID, Authentik, Authelia, ...).

Turned on by environment:

    OWNHEALTH_OIDC_ISSUER=https://id.example.lan          (discovery: <issuer>/.well-known/openid-configuration)
    OWNHEALTH_OIDC_CLIENT_ID=ownhealth
    OWNHEALTH_OIDC_CLIENT_SECRET=...                      (or OWNHEALTH_OIDC_CLIENT_SECRET_FILE; empty = public client)
    OWNHEALTH_OIDC_LABEL=Pocket ID                        (button text, optional)
    OWNHEALTH_OIDC_SCOPES="openid profile email"          (optional)

Register the redirect URI `<OWNHEALTH_BASE_URL>/api/auth/oidc/callback` at the provider.
Authorization code flow with PKCE (S256), `state` and `nonce`, each single use (`auth_challenge`).
The ID token's signature is checked against the provider's JWKS, plus iss, aud, exp and nonce.
A person is matched by `app_user.oidc_subject` = `<issuer>#<sub>`; an unknown subject logs in only
after it is linked (an invite redeemed through OIDC, or "link" from a logged-in session), except on
first run, when it becomes the owner.
"""

from __future__ import annotations

import base64
import hashlib
import os
import secrets
import time
from dataclasses import dataclass
from urllib.parse import urlencode
from uuid import UUID

import httpx
from joserfc import jwt
from joserfc.errors import JoseError
from joserfc.jwk import KeySet
from sqlalchemy import Connection

from ownhealth.auth import challenges


def _env(name: str) -> str | None:
    """OWNHEALTH_<name>, or the contents of the file named by OWNHEALTH_<name>_FILE."""
    path = os.environ.get(f"OWNHEALTH_{name}_FILE")
    if path:
        with open(path) as f:
            return f.read().strip()
    return os.environ.get(f"OWNHEALTH_{name}")


MODES = ("login", "link", "enroll", "step_up")
_ALGS = ["RS256", "RS384", "RS512", "ES256", "ES384", "PS256", "EdDSA"]


class OIDCError(ValueError):
    def __init__(self, code: str, message: str):
        super().__init__(message)
        self.code = code


@dataclass(frozen=True)
class OIDCConfig:
    issuer: str
    client_id: str
    client_secret: str | None = None
    label: str = "OIDC"
    scopes: str = "openid profile email"

    @classmethod
    def from_env(cls) -> OIDCConfig | None:
        issuer, client_id = _env("OIDC_ISSUER"), _env("OIDC_CLIENT_ID")
        if not issuer or not client_id:
            return None
        return cls(
            issuer=issuer.rstrip("/"),
            client_id=client_id,
            client_secret=_env("OIDC_CLIENT_SECRET") or None,
            label=_env("OIDC_LABEL") or "OIDC",
            scopes=_env("OIDC_SCOPES") or "openid profile email",
        )


@dataclass(frozen=True)
class Result:
    mode: str
    subject: str  # '<issuer>#<sub>'
    name: str | None
    auth_time: int | None
    challenge: challenges.Challenge


_discovery: dict[str, tuple[float, dict]] = {}


def discover(cfg: OIDCConfig, http: httpx.Client) -> dict:
    cached = _discovery.get(cfg.issuer)
    if cached and cached[0] > time.time():
        return cached[1]
    r = http.get(f"{cfg.issuer}/.well-known/openid-configuration", timeout=10)
    r.raise_for_status()
    meta = r.json()
    if meta.get("issuer", "").rstrip("/") != cfg.issuer:
        raise OIDCError("oidc_misconfigured", "The provider's issuer does not match OWNHEALTH_OIDC_ISSUER.")
    _discovery[cfg.issuer] = (time.time() + 3600, meta)
    return meta


def _s256(verifier: str) -> str:
    return base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()


def begin(
    conn: Connection,
    cfg: OIDCConfig,
    http: httpx.Client,
    *,
    mode: str,
    redirect_uri: str,
    user_id: UUID | None = None,
    session_id: UUID | None = None,
    data: dict | None = None,
) -> str:
    """The provider URL to send the browser to."""
    if mode not in MODES:
        raise OIDCError("bad_input", f"unknown mode {mode}")
    meta = discover(cfg, http)
    state, nonce, verifier = secrets.token_urlsafe(32), secrets.token_urlsafe(24), secrets.token_urlsafe(48)
    challenges.issue(
        conn,
        "oidc",
        state.encode(),
        user_id=user_id,
        session_id=session_id,
        data={**(data or {}), "mode": mode, "nonce": nonce, "verifier": verifier},
        ttl=challenges.TTL * 2,
    )
    params = {
        "response_type": "code",
        "client_id": cfg.client_id,
        "redirect_uri": redirect_uri,
        "scope": cfg.scopes,
        "state": state,
        "nonce": nonce,
        "code_challenge": _s256(verifier),
        "code_challenge_method": "S256",
    }
    if mode == "step_up":
        params.update(prompt="login", max_age="0")
    return f"{meta['authorization_endpoint']}?{urlencode(params)}"


def binder_hash(binder: str) -> str:
    """What the challenge keeps of the browser binder cookie set when the flow began."""
    return hashlib.sha256(binder.encode()).hexdigest()


def finish(
    conn: Connection,
    cfg: OIDCConfig,
    http: httpx.Client,
    *,
    state: str,
    code: str,
    redirect_uri: str,
    binder: str,
) -> Result:
    """Exchange the code, verify the ID token. Consumes the state.

    `binder` is the cookie the browser got when the flow began: a callback opened in another
    browser (a link or login URL someone else started and sent) is refused."""
    ch = challenges.consume(conn, state.encode(), ("oidc",))
    if not binder or not secrets.compare_digest(ch.data.get("binder") or "", binder_hash(binder)):
        raise OIDCError("oidc_failed", "This sign-in was started in another browser. Start again.")
    meta = discover(cfg, http)
    form = {
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "code_verifier": ch.data["verifier"],
        "client_id": cfg.client_id,
    }
    auth = (cfg.client_id, cfg.client_secret) if cfg.client_secret else None
    r = http.post(meta["token_endpoint"], data=form, auth=auth, timeout=10)
    if r.status_code != 200:
        raise OIDCError("oidc_failed", f"The provider refused the code ({r.status_code}).")
    id_token = r.json().get("id_token")
    if not id_token:
        raise OIDCError("oidc_failed", "The provider sent no ID token.")
    jwks = http.get(meta["jwks_uri"], timeout=10)
    jwks.raise_for_status()
    try:
        token = jwt.decode(id_token, KeySet.import_key_set(jwks.json()), algorithms=_ALGS)
        jwt.JWTClaimsRegistry(
            leeway=60,
            iss={"essential": True, "value": cfg.issuer},
            aud={"essential": True, "value": cfg.client_id},
            sub={"essential": True},
            exp={"essential": True},
            nonce={"essential": True, "value": ch.data["nonce"]},
        ).validate(token.claims)
    except (JoseError, ValueError) as exc:
        raise OIDCError("oidc_failed", f"The ID token is not valid: {exc}") from exc
    claims = token.claims
    name = claims.get("name") or claims.get("preferred_username") or claims.get("email")
    return Result(
        mode=ch.data["mode"],
        subject=f"{cfg.issuer}#{claims['sub']}",
        name=str(name)[:80] if name else None,
        auth_time=int(claims["auth_time"]) if claims.get("auth_time") is not None else None,
        challenge=ch,
    )
