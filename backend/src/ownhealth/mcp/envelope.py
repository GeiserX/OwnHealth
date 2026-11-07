"""The MCP response envelope (mcp.md 3, 9.1). `untrusted` is final and every lane uses it for free
text going out over MCP; the rest is lane F's contract, stubbed until it lands."""

from __future__ import annotations

from typing import Literal

Origin = Literal["pdf_text_layer", "ocr", "email_subject", "fhir_note", "agent_note", "person_note"]
ORIGINS: tuple[str, ...] = ("pdf_text_layer", "ocr", "email_subject", "fhir_note", "agent_note", "person_note")
NOTICE = "Information from your records, not a diagnosis."


def untrusted(text: str | None, origin: Origin) -> dict | None:
    """Wrap free text that did not come from our own templates. None stays None."""
    if text is None:
        return None
    if origin not in ORIGINS:
        raise ValueError(f"unknown untrusted_text origin {origin!r}")
    return {"trust": "untrusted_text", "origin": origin, "text": text}


class ToolError(Exception):
    """A tool execution error (`isError: true`) with a code, a one-line message and a fix hint."""

    def __init__(self, code: str, message: str, fix: str | None = None, **extra):
        super().__init__(message)
        self.code, self.message, self.fix, self.extra = code, message, fix, extra

    def as_dict(self) -> dict:
        return {"isError": True, "code": self.code, "message": self.message, "fix": self.fix, **self.extra}


def envelope(*, profile: str, as_of: str, lang: str, identity_mode: str, data: dict, **fields) -> dict:
    raise NotImplementedError("lane F: mcp.envelope.envelope")


def estimate_tokens(obj) -> int:
    """len(json)/3.5, the budget unit of mcp.md 3."""
    raise NotImplementedError("lane F: mcp.envelope.estimate_tokens")


def encode_cursor(secret: str, query_hash: str, as_of: str, last_key) -> str:
    raise NotImplementedError("lane F: mcp.envelope.encode_cursor")


def decode_cursor(secret: str, cursor: str, query_hash: str) -> dict:
    raise NotImplementedError("lane F: mcp.envelope.decode_cursor")
