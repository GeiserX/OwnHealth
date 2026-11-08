"""The synthetic household: Alex, Sam and Lucía Rivera plus Carmen Ortega. Never real people.

Used by the demo seed and by the test factory (`tests/conftest.py`), so every lane tests against
the same family and the same grants.

| Profile | Born       | Sex    | Login | Alex's role | Sam's role |
|---------|------------|--------|-------|-------------|------------|
| alex    | 1979-05-14 | male   | Alex  | owner       | viewer     |
| sam     | 1981-02-02 | female | Sam   | viewer      | owner      |
| lucia   | 2017-06-21 | female | none  | guardian    | guardian   |
| carmen  | 1955-01-30 | female | none  | caregiver   | (none)     |
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import date
from uuid import UUID

from sqlalchemy import Connection

from ownhealth.core.access import ProfileIn, create_household, create_profile, create_user, set_grant

AS_OF = date(2026, 11, 7)  # the date every example in the docs uses


@dataclass
class Family:
    household_id: UUID
    users: dict[str, UUID] = field(default_factory=dict)  # alex, sam
    profiles: dict[str, UUID] = field(default_factory=dict)  # alex, sam, lucia, carmen


def create_rivera_family(conn: Connection, household_name: str = "Rivera") -> Family:
    h = create_household(conn, household_name)
    fam = Family(household_id=h)
    fam.users["alex"] = create_user(conn, h, "Alex Rivera", locale="es")
    fam.users["sam"] = create_user(conn, h, "Sam Rivera", locale="en")
    people = [
        ("alex", "Alex Rivera", "self", date(1979, 5, 14), "male", fam.users["alex"]),
        ("sam", "Sam Rivera", "partner", date(1981, 2, 2), "female", fam.users["sam"]),
        ("lucia", "Lucía Rivera", "child", date(2017, 6, 21), "female", None),
        ("carmen", "Carmen Ortega", "parent", date(1955, 1, 30), "female", None),
    ]
    for slug, name, rel, born, sex, user in people:
        fam.profiles[slug] = create_profile(
            conn,
            ProfileIn(
                household_id=h,
                slug=slug,
                display_name=name,
                relationship=rel,
                birth_date=born,
                sex_at_birth=sex,
                country="ES",
                user_id=user,
            ),
        )
    a, s = fam.users["alex"], fam.users["sam"]
    p = fam.profiles
    set_grant(conn, a, p["alex"], "owner")
    set_grant(conn, a, p["sam"], "viewer", granted_by=s)
    set_grant(conn, a, p["lucia"], "guardian")
    set_grant(conn, a, p["carmen"], "caregiver")
    set_grant(conn, s, p["sam"], "owner")
    set_grant(conn, s, p["alex"], "viewer", granted_by=a)
    set_grant(conn, s, p["lucia"], "guardian")
    return fam
