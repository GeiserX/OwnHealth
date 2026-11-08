"""Fixtures for lane F: a committed Rivera household with labs, records, a note and genome findings,
a PAT factory, a real MCP client session against the ASGI app, and fakes for the lane services
(state, genome, normalize, pdf) that are still stubs. Synthetic data only.

The fakes are deliberately careless: the genome fake returns sensitive and minor-withheld findings
and the state fake returns gene x drug flags and medications to every caller, so the tests prove the
MCP layer itself keeps them away from tokens that may not see them.
"""

from __future__ import annotations

import hashlib
import json
import secrets
from contextlib import asynccontextmanager
from dataclasses import dataclass, field
from datetime import UTC, date, datetime
from decimal import Decimal
from uuid import UUID

import httpx2
import pytest
from factories import lab_document
from mcp.client.client import Client
from mcp.client.streamable_http import streamable_http_client
from sqlalchemy import text

from ownhealth.auth.tokens import create_token
from ownhealth.blobs import BlobStore
from ownhealth.core.access import ALL_SCOPES, set_genome_switches
from ownhealth.core.observations import ObservationIn, commit_observation
from ownhealth.core.provenance import SrcIn, ensure_src
from ownhealth.core.records import MedicationIn, active_medications, add_record
from ownhealth.demo.family import Family, create_rivera_family
from ownhealth.genome import Findings
from ownhealth.mcp.envelope import untrusted


@pytest.fixture
def anyio_backend():
    return "asyncio"


@dataclass
class World:
    family: Family
    src: dict[str, str] = field(default_factory=dict)
    doc_id: UUID | None = None
    boxes: list = field(default_factory=list)

    def pid(self, slug: str) -> UUID:
        return self.family.profiles[slug]


def _obs(
    profile_id,
    name,
    value,
    unit,
    group,
    loinc,
    day,
    *,
    low=None,
    high=None,
    flag=None,
    category="laboratory",
    sensitivity=(),
    document_id=None,
    value_text=None,
) -> ObservationIn:
    y, m, d = map(int, day.split("-"))
    num = Decimal(value) if value_text is None else None
    return ObservationIn(
        profile_id=profile_id,
        category=category,
        printed_name=name,
        printed_value=value if value_text is None else value_text,
        printed_unit=unit,
        printed_ref_range=(f"{low} - {high}" if low and high else (f"< {high}" if high else None)),
        value_num=num,
        value_text=value_text,
        ref_low=Decimal(low) if low else None,
        ref_high=Decimal(high) if high else None,
        loinc=loinc,
        alias_group=group,
        unit_ucum=unit,
        value_canonical=num,
        unit_canonical=unit if num is not None else None,
        interpretation=flag,
        derivation_version="test",
        effective_at=datetime(y, m, d, 8, tzinfo=UTC),
        sensitivity=list(sensitivity),
        document_id=document_id,
    )


def manual(profile_id) -> SrcIn:
    return SrcIn(kind="manual", profile_id=profile_id, actor_kind="system")


def add_obs(conn, profile_id, *args, src=None, printed: dict | None = None, **kw) -> str:
    o = _obs(profile_id, *args, **kw)
    if printed:
        o = o.model_copy(update=printed)
    return commit_observation(conn, o, src=src or manual(profile_id), accepted_by="person").src_id


def add_finding(
    conn,
    profile_id,
    dataset_id,
    fact_id,
    category,
    gene,
    *,
    sensitive=False,
    adult_onset=False,
    carrier=False,
    phenotype=None,
    diplotype=None,
    fact: dict | None = None,
) -> str:
    sid = ensure_src(conn, SrcIn(kind="genome_facts", profile_id=profile_id, import_ref=f"facts#{fact_id}"))
    conn.execute(
        text(
            "insert into genome_finding (dataset_id, profile_id, fact_id, category, gene, diplotype, phenotype,"
            " evidence, confidence, call_state, sensitive, adult_onset, carrier, status, src_id, fact) values"
            " (:d, :p, :f, :c, :g, :dip, :ph, 'E1', 'high', 'alt', :s, :a, :car, 'current', :src, :fact)"
        ),
        {
            "d": dataset_id,
            "p": profile_id,
            "f": fact_id,
            "c": category,
            "g": gene,
            "dip": diplotype,
            "ph": phenotype,
            "s": sensitive,
            "a": adult_onset,
            "car": carrier,
            "src": sid,
            "fact": json.dumps(fact or {}),
        },
    )
    return sid


def add_dataset(conn, profile_id) -> UUID:
    sid = ensure_src(conn, SrcIn(kind="genome_facts", profile_id=profile_id, import_ref="facts"))
    return conn.execute(
        text(
            "insert into genome_dataset (profile_id, sample_id, assay, facts_schema, facts_sha256, facts_blob,"
            " owngenome_version, db_versions, src_id) values (:p, 'S1', 'wgs', 'ownhealth.genome-facts/v1', :h, :h,"
            " '1.4.2', '{\"clinvar\": \"2026-10-28\"}', :s) returning id"
        ),
        {"p": profile_id, "h": secrets.token_bytes(32), "s": sid},
    ).scalar_one()


def fact_hex() -> str:
    return "f_" + secrets.token_hex(8)


@pytest.fixture
def world(engine, settings) -> World:
    blobs = BlobStore(settings.blobs_path)
    with engine.begin() as conn:
        fam = create_rivera_family(conn, f"Rivera {secrets.token_hex(3)}")
        w = World(family=fam)
        alex, lucia = fam.profiles["alex"], fam.profiles["lucia"]
        for day, v in (
            ("2021-11-09", "163"),
            ("2022-09-15", "96"),
            ("2023-03-21", "88"),
            ("2025-01-20", "79"),
            ("2025-09-08", "101"),
            ("2026-03-02", "118"),
        ):
            w.src[f"ldl_{day}"] = add_obs(
                conn,
                alex,
                "Colesterol LDL",
                v,
                "mg/dL",
                "ldl_c",
                "13457-7",
                day,
                high="130",
                flag="H" if int(v) > 130 else "N",
            )
        w.src["ldl_direct"] = add_obs(
            conn, alex, "LDL directo", "92", "mg/dL", "ldl_c", "18262-6", "2023-10-04", high="130", flag="N"
        )
        doc_id, boxes = lab_document(conn, blobs, fam)
        w.doc_id, w.boxes = doc_id, boxes
        conn.execute(
            text("update document set collected_at = :c, reported_at = :c, provider_name = 'LabNorte' where id = :d"),
            {"c": datetime(2026, 9, 14, 8, tzinfo=UTC), "d": doc_id},
        )
        region = SrcIn(kind="document_region", profile_id=alex, document_id=doc_id, page_no=1, bbox=boxes[1])
        w.src["vitd"] = add_obs(
            conn,
            alex,
            "Vitamina D",
            "24",
            "ng/mL",
            "vitamin_d_25oh",
            "62292-8",
            "2026-09-14",
            low="30",
            high="100",
            flag="L",
            src=region,
            document_id=doc_id,
        )
        region0 = SrcIn(kind="document_region", profile_id=alex, document_id=doc_id, page_no=1, bbox=boxes[0])
        w.src["ldl_latest"] = add_obs(
            conn,
            alex,
            "Colesterol LDL",
            "97",
            "mg/dL",
            "ldl_c",
            "13457-7",
            "2026-09-14",
            high="130",
            flag="N",
            src=region0,
            document_id=doc_id,
        )
        w.src["weight"] = add_obs(
            conn, alex, "Peso", "78.5", "kg", "body_weight", "29463-7", "2026-10-01", category="vital-signs"
        )
        w.src["phq9"] = add_obs(
            conn,
            alex,
            "PHQ-9",
            "6",
            "{score}",
            "phq9",
            "44261-6",
            "2026-06-01",
            category="survey",
            sensitivity=("mental_health",),
        )
        med = add_record(
            conn,
            MedicationIn(profile_id=alex, name="Atorvastatin 20 mg", started=date(2026, 3, 10)),
            src=manual(alex),
            accepted_by="person",
        )
        w.src["statin"] = med.src_id
        note_src = ensure_src(conn, manual(alex))
        conn.execute(
            text(
                "insert into note (profile_id, kind, author_kind, lang, attach_to, body, effective_at, src_id)"
                " values (:p, 'priorities', 'person', 'en', 'status', 'Recheck vitamin D in winter.', now(), :s)"
            ),
            {"p": alex, "s": note_src},
        )
        w.src["note"] = note_src
        ds = add_dataset(conn, alex)
        w.src["slco1b1"] = add_finding(
            conn, alex, ds, fact_hex(), "pgx", "SLCO1B1", phenotype="Decreased function", diplotype="*1/*5"
        )
        w.src["apoe"] = add_finding(
            conn,
            alex,
            ds,
            fact_hex(),
            "risk_marker",
            "APOE",
            sensitive=True,
            adult_onset=True,
            fact={"variants": [{"chrom": "chr19", "pos": 44908684, "ref": "T", "alt": "C", "rsid": "rs429358"}]},
        )
        w.src["hfe"] = add_finding(conn, alex, ds, fact_hex(), "carrier", "HFE", carrier=True)
        ds_l = add_dataset(conn, lucia)
        w.src["lucia_pgx"] = add_finding(
            conn, lucia, ds_l, fact_hex(), "pgx", "CYP2C19", phenotype="Normal metabolizer"
        )
        w.src["lucia_carrier"] = add_finding(conn, lucia, ds_l, fact_hex(), "carrier", "CFTR", carrier=True)
        w.src["sam_ldl"] = add_obs(
            conn, fam.profiles["sam"], "Colesterol LDL", "110", "mg/dL", "ldl_c", "13457-7", "2026-08-01", high="130"
        )
    return w


@pytest.fixture
def make_token(engine, world):
    """make_token(scopes=..., profiles=('alex',), user='alex', **kw) -> plaintext PAT (committed)."""

    def _make(scopes=ALL_SCOPES, profiles=("alex",), user="alex", **kw) -> str:
        with engine.begin() as conn:
            tok = create_token(
                conn,
                user_id=world.family.users[user],
                label=kw.pop("label", "Claude Code · test"),
                profile_ids=[world.pid(p) for p in profiles],
                scopes=sorted(scopes),
                lang=kw.pop("lang", "en"),
                identity_mode=kw.pop("identity_mode", "full"),
                **kw,
            )
        return tok.plaintext

    return _make


def token_id(plaintext: str):
    from ownhealth.auth.tokens import PREFIX

    return PREFIX + plaintext[len(PREFIX) :].split("_", 1)[0]


@pytest.fixture
def make_app(settings, engine):
    """A fresh app each time: the SDK's session manager runs once per app."""
    from ownhealth.app import create_app

    return lambda: create_app(settings, engine)


@pytest.fixture
def connect(make_app):
    """async with connect(token, mode=...) as client: a real MCP client session over streamable HTTP."""

    @asynccontextmanager
    async def _connect(token: str | None, mode: str = "2026-07-28", headers: dict | None = None):
        app = make_app()
        hdrs = {"Authorization": f"Bearer {token}"} if token else {}
        hdrs.update(headers or {})
        async with (
            app.router.lifespan_context(app),
            httpx2.AsyncClient(
                transport=httpx2.ASGITransport(app=app), base_url="http://testserver", headers=hdrs
            ) as hc,
            Client(streamable_http_client("http://testserver/mcp", http_client=hc), mode=mode) as c,
        ):
            yield c

    return _connect


@pytest.fixture
def raw_http(make_app):
    """async with raw_http() as hc: a plain HTTP client on the ASGI app (no MCP session)."""

    @asynccontextmanager
    async def _raw():
        app = make_app()
        transport = httpx2.ASGITransport(app=app)
        async with (
            app.router.lifespan_context(app),
            httpx2.AsyncClient(transport=transport, base_url="http://testserver") as hc,
        ):
            yield hc

    return _raw


# ------------------------------------------------------------- lane fakes ----


def _canon(obj) -> bytes:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"), default=str).encode()


def fake_build_state(conn, profile_id, *, as_of):
    rows = (
        conn.execute(
            text(
                "select * from observation where profile_id = :p and status = 'current' and interpretation in ('H','L')"
                " and effective_at::date <= :d order by effective_at desc"
            ),
            {"p": profile_id, "d": as_of},
        )
        .mappings()
        .all()
    )
    flags = [
        {
            "kind": "out_of_range",
            "reason": "LAB_HIGH" if r["interpretation"] == "H" else "LAB_LOW",
            "code": f"LOINC:{r['loinc']}",
            "alias_group": r["alias_group"],
            "value": str(r["value_num"]),
            "unit": r["unit_ucum"],
            "flag": r["interpretation"],
            "date": r["effective_at"].date().isoformat(),
            "src": r["src_id"],
        }
        for r in rows
    ]
    meds = active_medications(conn, profile_id, on=as_of)
    gene = conn.execute(
        text("select src_id from genome_finding where profile_id = :p and gene = 'SLCO1B1'"), {"p": profile_id}
    ).scalar()
    if gene and meds:
        flags.append(
            {
                "kind": "gene_drug",
                "reason": "GENE_DRUG",
                "gene": "SLCO1B1",
                "drug": "atorvastatin",
                "src": [gene, meds[0]["src_id"]],
            }
        )
    notes = conn.execute(text("select body, src_id from note where profile_id = :p"), {"p": profile_id}).all()
    body = {
        "schema": "ownhealth.state/v1",
        "rules": "ownhealth-rules 0.1.0",
        "as_of": as_of.isoformat(),
        "flags": [{"rank": i + 1, **f} for i, f in enumerate(flags)],
        "medications": [{"name": m["name"], "since": m["started"], "src": m["src_id"]} for m in meds],
        "never_measured": [{"alias_group": "hcv_ab", "rule": "infect.hcv.once_adult"}],
        "stale": [],
        "notes": [{"author_kind": "person", "body": n.body, "src": n.src_id} for n in notes],
        "data_freshness": {"labs": "2026-09-14", "genome": "2026-10-02"},
    }
    body["state_hash"] = "sha256:" + hashlib.sha256(_canon(body)).hexdigest()
    return body


def fake_render(body, *, lang, units="as_reported", identity_mode="pseudonymous"):
    out = {k: v for k, v in body.items() if k not in ("schema", "rules", "as_of")}
    out["rules_version"] = body["rules"]
    word = {"en": "Out of the lab's range", "es": "Fuera del rango del laboratorio"}[lang]
    out["flags"] = [{**f, "text": word} if f["kind"] == "out_of_range" else f for f in body["flags"]]
    out["notes"] = [
        {"author_kind": n["author_kind"], "text": untrusted(n["body"], "person_note"), "src": n["src"]}
        for n in body["notes"]
    ]
    out["limitations"] = ["Flags use each lab's own printed range."]
    return out


def fake_list_findings(conn, profile_id, *, gate, category=None, gene=None, min_evidence="E2", as_of=None):
    """Careless on purpose: ignores the gate, so the MCP fact gate must catch what leaks."""
    rows = (
        conn.execute(
            text(
                "select * from genome_finding where profile_id = :p and status = 'current'"
                " and (cast(:c as text) is null or category = :c) and (cast(:g as text) is null or gene = :g)"
                " order by gene"
            ),
            {"p": profile_id, "c": category, "g": gene},
        )
        .mappings()
        .all()
    )
    return Findings(
        findings=[
            {
                "id": r["fact_id"],
                "category": r["category"],
                "gene": r["gene"],
                "diplotype": r["diplotype"],
                "phenotype": r["phenotype"],
                "evidence": r["evidence"],
                "confidence": r["confidence"],
                "call_state": r["call_state"],
                "src": r["src_id"],
            }
            for r in rows
        ],
        not_assessed=[{"gene": "CYP2D6", "what": "structural variants", "reason": "no read data in source"}],
        db_versions={"clinvar": "2026-10-28"},
        assay={"type": "wgs", "build": "GRCh38"},
    )


def fake_lookup_variant(conn, blobs, profile_id, *, gate, rsid=None, hgvs=None, region=None, cursor=None):
    src = conn.execute(text("select src_id from genome_dataset where profile_id = :p"), {"p": profile_id}).scalar()
    if region:
        page = int(cursor or 0)
        calls = [{"source": "wgs", "call_state": "ref", "pos": 1000 + i, "src": src} for i in range(page, page + 2)]
        return {"query": region, "calls": calls, "next_cursor": str(page + 2) if page < 2 else None}
    return {
        "query": rsid or hgvs,
        "variant": {"gene": "HFE", "chrom": "chr6", "pos": 26092913},
        "calls": [{"source": "wgs", "call_state": "alt", "genotype": "G/A", "src": src}],
        "db_versions": {"clinvar": "2026-10-28"},
    }


def fake_check_drug(conn, profile_id, drug, *, scopes, lang="en", as_of=None):
    """Careless on purpose: returns every part whatever the scopes."""
    g = conn.execute(
        text("select src_id from genome_finding where profile_id = :p and gene = 'SLCO1B1'"), {"p": profile_id}
    ).scalar()
    meds = active_medications(conn, profile_id, on=as_of)
    return {
        "drug": {"name": drug.title(), "cpic_drug_id": "RxNorm:83367", "currently_taking": bool(meds)},
        "pgx": [{"gene": "SLCO1B1", "phenotype": "Decreased function", "call_state": "alt", "src": g}] if g else [],
        "phenoconversion": [],
        "current_medications_relevant": [{"name": m["name"], "src": m["src_id"]} for m in meds],
        "relevant_labs": [],
        "guidance": [
            {
                "source": "CPIC",
                "applies_to": "Decreased function",
                "quote": "Prescribe a lower dose.",
                "url": "https://cpicpgx.org/guidelines/",
            }
        ],
        "not_assessed": ["drug-drug interactions (no open-licence source configured)"],
        "db_versions": {"cpic": "2026-09"},
    }


@pytest.fixture
def fakes(monkeypatch):
    from ownhealth import genome, normalize, state
    from ownhealth.ingest import pdf

    monkeypatch.setattr(state, "build_state", fake_build_state)
    monkeypatch.setattr(state, "render", fake_render)
    monkeypatch.setattr(state, "check_drug", fake_check_drug)
    monkeypatch.setattr(
        state,
        "resolve_drug",
        lambda conn, name: {"cpic_drug_id": "RxNorm:83367", "name": name} if "statin" in name.lower() else None,
    )
    monkeypatch.setattr(state, "rules_version", lambda: "ownhealth-rules 0.1.0")
    monkeypatch.setattr(genome, "list_findings", fake_list_findings)
    monkeypatch.setattr(genome, "lookup_variant", fake_lookup_variant)

    def resolve_analyte(conn, printed_name, *, lang, unit, specimen=None, method=None, provider=None):
        return None

    def normalize_row(
        conn,
        *,
        profile_id,
        printed_name,
        printed_value,
        printed_unit,
        printed_range,
        printed_flag,
        effective_at,
        lang,
        specimen=None,
        provider=None,
    ):
        from ownhealth.core.acceptance import Check
        from ownhealth.normalize import Normalized

        value = Decimal(str(printed_value).replace(",", "."))
        obs = ObservationIn(
            profile_id=profile_id,
            printed_name=printed_name,
            printed_value=printed_value,
            printed_unit=printed_unit,
            value_num=value,
            alias_group="ferritin",
            loinc="2276-4",
            unit_ucum=printed_unit,
            value_canonical=value,
            unit_canonical=printed_unit,
            derivation_version="test",
            effective_at=effective_at,
        )
        plausible = value < 100000
        return Normalized(
            observation=obs, checks=[Check(name="plausible", ok=plausible, reason=None if plausible else "implausible")]
        )

    def ground_value(conn, blobs, *, document_id, page_no, name, value):
        words = {"97": (300.0, 120.0, 320.0, 129.0), "24": (300.0, 136.0, 320.0, 145.0)}
        return words.get(str(value).replace(",", "."))

    def page_words(conn, blobs, *, document_id, page_no):
        return [pdf.Word(text="Colesterol", bbox=(56, 120, 100, 129)), pdf.Word(text="LDL", bbox=(102, 120, 120, 129))]

    monkeypatch.setattr(normalize, "resolve_analyte", resolve_analyte)
    monkeypatch.setattr(normalize, "normalize_row", normalize_row)
    monkeypatch.setattr(pdf, "ground_value", ground_value)
    monkeypatch.setattr(pdf, "page_words", page_words)


def set_switch(engine, profile_id, on: bool) -> None:
    with engine.begin() as conn:
        set_genome_switches(conn, profile_id, agents=on)
