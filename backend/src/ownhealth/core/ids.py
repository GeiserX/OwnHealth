"""Short public ids. `src_` ids are the citation ids shown over MCP (^src_[a-z0-9_]+$)."""

from __future__ import annotations

import re
import secrets

_ALPHABET = "abcdefghijkmnpqrstuvwxyz23456789"  # no l, o, 0, 1
SRC_ID = re.compile(r"^src_[a-z0-9_]+$")


def short(n: int = 8) -> str:
    return "".join(secrets.choice(_ALPHABET) for _ in range(n))


def new_src_id() -> str:
    return "src_" + short(8)


def is_src_id(value: str) -> bool:
    return bool(SRC_ID.match(value))
