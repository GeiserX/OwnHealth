"""RFC 8785 canonical JSON: the bytes the state hash is taken over."""

from datetime import date
from decimal import Decimal
from uuid import UUID

import pytest

from ownhealth.state import canonical_json, state_hash


def test_keys_sorted_no_whitespace():
    assert canonical_json({"b": 1, "a": [True, None, "x"]}) == b'{"a":[true,null,"x"],"b":1}'


def test_keys_sort_by_utf16_code_units():
    # RFC 8785 3.2.3: in UTF-16 the emoji is the surrogate pair D83D DE00, so it sorts between
    # U+20AC and U+FB33; by code point it would sort last.
    obj = {"דּ": 1, "\U0001f600": 2, "€": 3}
    assert canonical_json(obj).decode() == '{"€":3,"\U0001f600":2,"דּ":1}'


def test_decimal_keeps_printed_precision_and_types_become_strings():
    out = canonical_json(
        {"v": Decimal("0.90"), "d": date(2026, 11, 7), "u": UUID("00000000-0000-0000-0000-000000000001")}
    )
    assert out == b'{"d":"2026-11-07","u":"00000000-0000-0000-0000-000000000001","v":"0.90"}'


def test_unicode_is_not_escaped():
    assert canonical_json({"n": "Lucía"}) == '{"n":"Lucía"}'.encode()


def test_floats_are_refused():
    with pytest.raises(TypeError):
        canonical_json({"v": 0.1})


def test_state_hash_ignores_its_own_field_and_sees_every_other_change():
    body = {"a": "1", "flags": [{"src": "src_a12"}]}
    h = state_hash(body)
    assert h.startswith("sha256:") and len(h) == 71
    assert state_hash({**body, "state_hash": "sha256:whatever"}) == h
    assert state_hash({**body, "flags": [{"src": "src_a13"}]}) != h
