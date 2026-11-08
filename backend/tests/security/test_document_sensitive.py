"""The original file, page renders and words of a document holding a sensitive value need read:sensitive:
otherwise they would show the values every other read path hides."""

from factories import obs

from ownhealth.core.documents import create_document, document_is_sensitive
from ownhealth.core.observations import commit_observation
from ownhealth.core.provenance import SrcIn

PDF = b"%PDF-1.4\n% synthetic LabNorte slip\n%%EOF\n"


def _doc_with(engine, blobs, fam, who, sensitivity):
    pid = fam.profiles[who]
    with engine.begin() as c:
        ref = create_document(
            c,
            household_id=fam.household_id,
            profile_id=pid,
            blob_sha256=blobs.put_bytes(PDF + str(sensitivity).encode()),
            mime="application/pdf",
            size_bytes=len(PDF),
            filename="labnorte.pdf",
            pages=1,
            doc_type="lab_report",
        )
        doc_id = getattr(ref, "id", ref)
        o = obs(pid, printed_name="VIH Ac", document_id=doc_id, sensitivity=sensitivity)
        commit_observation(c, o, src=SrcIn(kind="manual", profile_id=pid, actor_kind="system"), accepted_by="import")
    return doc_id


def test_caregiver_cannot_fetch_a_sensitive_document(client_for, engine, committed_family):
    fam = committed_family
    alex = client_for(fam.users["alex"])  # caregiver on carmen: read:labs, no read:sensitive
    blobs = alex.app.state.runtime.blobs
    sensitive = _doc_with(engine, blobs, fam, "carmen", ["sexual_health"])
    plain = _doc_with(engine, blobs, fam, "carmen", [])
    with engine.connect() as c:
        assert document_is_sensitive(c, sensitive) and not document_is_sensitive(c, plain)
    r = alex.get(f"/api/documents/{sensitive}/file")
    assert r.status_code == 403 and r.json()["detail"]["code"] == "insufficient_scope", r.text
    assert alex.get(f"/api/documents/{sensitive}/pages/1.png").status_code == 403
    assert alex.get(f"/api/documents/{plain}/file").status_code == 200


def test_search_without_read_sensitive_neither_matches_nor_quotes_a_sensitive_page(
    client_for, engine, committed_family
):
    from sqlalchemy import text

    fam = committed_family
    alex = client_for(fam.users["alex"])  # caregiver on carmen; given read:documents_text, not read:sensitive
    blobs = alex.app.state.runtime.blobs
    sensitive = _doc_with(engine, blobs, fam, "carmen", ["sexual_health"])
    plain = _doc_with(engine, blobs, fam, "carmen", [])
    with engine.begin() as c:
        for d in (sensitive, plain):
            c.execute(
                text(
                    "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok,"
                    " text_content, fts) values (:d, 1, 595, 842, true, :t, to_tsvector('simple', :t))"
                ),
                {"d": d, "t": "LabNorte serologia VIH resultado reactivo"},
            )
        c.execute(
            text(
                "update profile_grant set categories = array_append(categories, 'read:documents_text')"
                " where user_id = :u and profile_id = :p"
            ),
            {"u": fam.users["alex"], "p": fam.profiles["carmen"]},
        )
    items = alex.get("/api/profiles/carmen/documents", params={"q": "serologia"}).json()["items"]
    assert [i["id"] for i in items] == [str(plain)]
    assert "serologia" in items[0]["snippet"]["text"].lower()
    # the filename still matches for every document, without a snippet from the sensitive one
    items = alex.get("/api/profiles/carmen/documents", params={"q": "labnorte"}).json()["items"]
    by_id = {i["id"]: i for i in items}
    assert set(by_id) == {str(sensitive), str(plain)}
    assert by_id[str(sensitive)]["snippet"] is None
    # the owner of the data (Alex on his own profile has read:sensitive) is unaffected
    mine = _doc_with(engine, blobs, fam, "alex", ["sexual_health"])
    with engine.begin() as c:
        c.execute(
            text(
                "insert into document_page (document_id, page_no, width_pt, height_pt, text_layer_ok,"
                " text_content, fts) values (:d, 1, 595, 842, true, :t, to_tsvector('simple', :t))"
            ),
            {"d": mine, "t": "LabNorte serologia VIH"},
        )
    items = alex.get("/api/profiles/alex/documents", params={"q": "serologia"}).json()["items"]
    assert [i["id"] for i in items] == [str(mine)] and items[0]["snippet"] is not None


def test_document_bytes_are_never_kept_in_the_browser_cache(client_for, engine, committed_family):
    fam = committed_family
    alex = client_for(fam.users["alex"])
    doc = _doc_with(engine, alex.app.state.runtime.blobs, fam, "alex", [])
    r = alex.get(f"/api/documents/{doc}/file")
    assert r.status_code == 200
    assert "no-store" in r.headers["cache-control"], r.headers["cache-control"]
