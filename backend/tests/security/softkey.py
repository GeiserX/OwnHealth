"""A software WebAuthn authenticator for tests: P-256, 'none' attestation, user verification on."""

from __future__ import annotations

import base64
import hashlib
import json
import os
import struct

import cbor2
from cryptography.hazmat.primitives import hashes
from cryptography.hazmat.primitives.asymmetric import ec


def b64u(b: bytes) -> str:
    return base64.urlsafe_b64encode(b).rstrip(b"=").decode()


def unb64u(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


class SoftKey:
    def __init__(self, rp_id: str = "testserver", origin: str = "https://testserver"):
        self.rp_id, self.origin = rp_id, origin
        self.key = ec.generate_private_key(ec.SECP256R1())
        self.cred_id = os.urandom(32)
        self.count = 0
        self.user_handle: bytes | None = None

    def _client(self, kind: str, challenge: str, origin: str | None) -> bytes:
        return json.dumps(
            {"type": kind, "challenge": challenge, "origin": origin or self.origin, "crossOrigin": False}
        ).encode()

    def _auth_data(self, flags: int, extra: bytes = b"", rp_id: str | None = None) -> bytes:
        self.count += 1
        return (
            hashlib.sha256((rp_id or self.rp_id).encode()).digest()
            + bytes([flags])
            + struct.pack(">I", self.count)
            + extra
        )

    def create(self, options: dict, *, origin: str | None = None) -> dict:
        self.user_handle = unb64u(options["user"]["id"])
        nums = self.key.public_key().public_numbers()
        cose = cbor2.dumps({1: 2, 3: -7, -1: 1, -2: nums.x.to_bytes(32, "big"), -3: nums.y.to_bytes(32, "big")})
        attested = b"\0" * 16 + struct.pack(">H", len(self.cred_id)) + self.cred_id + cose
        auth = self._auth_data(0x01 | 0x04 | 0x40, attested)
        att = cbor2.dumps({"fmt": "none", "attStmt": {}, "authData": auth})
        return {
            "id": b64u(self.cred_id),
            "rawId": b64u(self.cred_id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64u(self._client("webauthn.create", options["challenge"], origin)),
                "attestationObject": b64u(att),
                "transports": ["internal"],
            },
            "clientExtensionResults": {},
        }

    def get(self, options: dict, *, origin: str | None = None, rp_id: str | None = None) -> dict:
        client = self._client("webauthn.get", options["challenge"], origin)
        auth = self._auth_data(0x01 | 0x04, rp_id=rp_id)
        sig = self.key.sign(auth + hashlib.sha256(client).digest(), ec.ECDSA(hashes.SHA256()))
        return {
            "id": b64u(self.cred_id),
            "rawId": b64u(self.cred_id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64u(client),
                "authenticatorData": b64u(auth),
                "signature": b64u(sig),
                "userHandle": b64u(self.user_handle or b""),
            },
            "clientExtensionResults": {},
        }
