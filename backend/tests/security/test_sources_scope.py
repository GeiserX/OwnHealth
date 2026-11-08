"""GET /api/sources/{id} follows the scope and sensitivity of the fact behind the source."""

from factories import obs

from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn


def _src(engine, fam, who="alex", **kw):
    pid = fam.profiles[who]
    with engine.begin() as c:
        src = SrcIn(kind="manual", profile_id=pid, label="LabNorte", actor_kind="system")
        return commit_observation(c, obs(pid, **kw), src=src, accepted_by="import").src_id


def test_viewer_cannot_resolve_lab_or_sensitive_sources(client_for, engine, committed_family):
    fam = committed_family
    lab = _src(engine, fam)
    phq9 = _src(
        engine, fam, value="12", group="phq9", loinc="44261-6", category="survey", sensitivity=["mental_health"]
    )
    sam = client_for(fam.users["sam"])  # viewer on alex: read:summary only
    r = sam.get(f"/api/sources/{lab}")
    assert r.status_code == 403 and r.json()["detail"]["code"] == "insufficient_scope", r.text
    assert "document" not in r.text
    r = sam.get(f"/api/sources/{phq9}")
    assert r.status_code in (403, 404) and "facts" not in r.text, r.text
    # a caregiver has read:vitals on carmen but not read:sensitive: the sensitive source is unknown
    carmen = _src(
        engine,
        fam,
        "carmen",
        value="12",
        group="phq9",
        loinc="44261-6",
        category="survey",
        sensitivity=["mental_health"],
    )
    r = client_for(fam.users["alex"]).get(f"/api/sources/{carmen}")
    assert r.status_code == 404 and r.json()["detail"]["code"] == "unknown", r.text
    owner = client_for(fam.users["alex"])
    assert owner.get(f"/api/sources/{lab}").status_code == 200
    assert owner.get(f"/api/sources/{phq9}").status_code == 200
