from datetime import date

import pytest

from ownhealth.core.access import (
    ALL_SCOPES,
    AccessDenied,
    Principal,
    accessible_profiles,
    can,
    effective_permission,
    effective_scopes,
    require,
    resolve_profile_ref,
    set_grant,
    set_locks,
)


def session(fam, who="alex"):
    return Principal(kind="session", user_id=fam.users[who], household_id=fam.household_id)


def test_effective_scopes_formula():
    token = {"read:summary", "read:labs", "read:sensitive"}
    cats = {"read:summary", "read:labs", "read:sensitive", "read:genome"}
    locks = {"read:sensitive"}
    assert effective_scopes(token, cats, locks, "caregiver") == {"read:summary", "read:labs"}
    # the owner's own grant ignores locks
    assert effective_scopes(token, cats, locks, "owner") == {"read:summary", "read:labs", "read:sensitive"}
    # token scopes cap everything
    assert effective_scopes({"read:summary"}, ALL_SCOPES, set(), "owner") == {"read:summary"}


def test_role_actions():
    assert can("owner", "manage_grants") and can("owner", "export")
    assert can("guardian", "import") and not can("guardian", "export")
    assert can("caregiver", "review") and not can("caregiver", "manage_grants")
    assert not can("viewer", "import") and not can("viewer", "write")


def test_family_grants(conn, family):
    alex = session(family)
    perms = {p.profile.slug: p for p in accessible_profiles(conn, alex)}
    assert set(perms) == {"alex", "sam", "lucia", "carmen"}
    assert perms["alex"].role == "owner" and perms["alex"].scopes == ALL_SCOPES
    assert perms["sam"].scopes == {"read:summary"}
    assert perms["carmen"].scopes == {"read:summary", "read:labs", "read:vitals", "read:records", "write:propose"}
    assert perms["lucia"].profile.is_minor(date(2026, 11, 7))
    sam = session(family, "sam")
    assert {p.profile.slug for p in accessible_profiles(conn, sam)} == {"alex", "sam", "lucia"}
    with pytest.raises(AccessDenied) as e:
        effective_permission(conn, sam, family.profiles["carmen"])
    assert e.value.code == "profile_not_granted"


def test_locks_bind_everyone_but_the_owner(conn, family):
    set_grant(conn, family.users["sam"], family.profiles["alex"], "viewer", {"read:summary", "read:sensitive"})
    set_locks(conn, family.profiles["alex"], {"read:sensitive"})
    sam_view = effective_permission(conn, session(family, "sam"), family.profiles["alex"])
    assert sam_view.scopes == {"read:summary"}
    alex_own = effective_permission(conn, session(family), family.profiles["alex"])
    assert "read:sensitive" in alex_own.scopes


def test_token_limits_profiles_and_scopes(conn, family):
    token = Principal(
        kind="token",
        user_id=family.users["alex"],
        household_id=family.household_id,
        scopes=frozenset({"read:summary", "read:labs"}),
        profile_ids=frozenset({family.profiles["alex"]}),
        default_profile=family.profiles["alex"],
    )
    assert resolve_profile_ref(conn, token, None) == family.profiles["alex"]
    perm = require(conn, token, "alex", scope="read:labs")
    assert perm.scopes == {"read:summary", "read:labs"}
    with pytest.raises(AccessDenied) as e:
        require(conn, token, "alex", scope="read:genome")
    assert e.value.code == "insufficient_scope"
    with pytest.raises(AccessDenied) as e:
        require(conn, token, "carmen")
    assert e.value.code == "profile_not_granted" and e.value.allowed == ["alex"]


def test_me_and_actions(conn, family):
    alex = session(family)
    assert resolve_profile_ref(conn, alex, "me") == family.profiles["alex"]
    with pytest.raises(AccessDenied):
        require(conn, alex, "sam", action="import")  # viewer
    assert require(conn, alex, "carmen", action="import").role == "caregiver"


def test_an_ungranted_uuid_reads_like_an_unknown_one(conn, family):
    from uuid import uuid4

    sam = session(family, "sam")
    token = Principal(
        kind="token",
        user_id=family.users["alex"],
        household_id=family.household_id,
        scopes=ALL_SCOPES,
        profile_ids=frozenset({family.profiles["alex"]}),
    )
    messages = set()
    for who, pid in ((sam, family.profiles["carmen"]), (token, family.profiles["sam"]), (sam, uuid4())):
        with pytest.raises(AccessDenied) as e:
            effective_permission(conn, who, pid)
        assert e.value.code == "profile_not_granted"
        messages.add(str(e.value))
    assert len(messages) == 1 and "carmen" not in messages.pop()
