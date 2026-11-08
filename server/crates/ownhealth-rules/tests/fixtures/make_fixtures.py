"""Write the lane E parity fixtures from the Python backend 0.1.1 (the reference).

    cd backend && .venv/bin/python ../server/crates/ownhealth-rules/tests/fixtures/make_fixtures.py

Seeds a throwaway database in the shared test Postgres with the Rivera family and the base demo data
(synthetic only), dumps its rows to `seed.sql`, then records what the Python state code answers for
it in `expected.json`: state bodies with their state_hash, rendered status, dashboards, check_drug and
status words. Genome findings come from the same stand-in as backend/tests/state/conftest.py. The Rust
tests load `seed.sql` into their own database and must give the same bytes.
"""

from __future__ import annotations

import json
import os
import secrets
import subprocess
import sys
import tempfile
from datetime import date
from pathlib import Path

from sqlalchemy import text

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parents[4] / "backend"))  # tests.state.conftest

from ownhealth import genome, state  # noqa: E402
from ownhealth.blobs import BlobStore  # noqa: E402
from ownhealth.config import Settings  # noqa: E402
from ownhealth.core.access import ALL_SCOPES  # noqa: E402
from ownhealth.db import make_engine  # noqa: E402
from ownhealth.db.migrate import migrate  # noqa: E402
from ownhealth.demo import DemoContext  # noqa: E402
from ownhealth.demo.base import seed_base  # noqa: E402
from ownhealth.demo.family import create_rivera_family  # noqa: E402
from ownhealth.state.pack import cpic_snapshot, load_pack  # noqa: E402
from tests.state.conftest import AS_OF, FINDINGS  # noqa: E402

ADMIN = os.environ.get("OWNHEALTH_TEST_ADMIN_URL", "postgresql://postgres:postgres@127.0.0.1:55432/postgres")
SLUGS = ("alex", "sam", "lucia", "carmen")


def main() -> None:
    name = f"oh_fixture_{secrets.token_hex(4)}"
    admin = make_engine(ADMIN, isolation_level="AUTOCOMMIT")
    with admin.connect() as c:
        c.execute(text(f'create database "{name}"'))
    try:
        run(name)
    finally:
        with admin.connect() as c:
            c.execute(text(f'drop database if exists "{name}" with (force)'))


def run(name: str) -> None:
    eng = make_engine(ADMIN.rsplit("/", 1)[0] + "/" + name)
    migrate(eng)
    tmp = Path(tempfile.mkdtemp())
    settings = Settings(database_url=str(eng.url), data_dir=tmp)
    with eng.begin() as conn:
        family = create_rivera_family(conn, "Rivera (lane E)")
        seed_base(conn, DemoContext(settings=settings, blobs=BlobStore(tmp / "blobs"), family=family))
    dump = subprocess.run(
        ["docker", "exec", "ownhealth-testpg", "pg_dump", "-U", "postgres", "--data-only", "--column-inserts",
         "--no-owner", "--exclude-table=schema_migrations", "-d", name],
        check=True, capture_output=True, text=True,
    ).stdout  # fmt: skip
    # rows only: no SET, no comments, no psql meta-commands; a string value may span lines
    skip = ("SET ", "SELECT pg_catalog.", "--", "\\")
    lines = [ln for ln in dump.splitlines() if ln and not ln.startswith(skip)]
    (HERE / "seed.sql").write_text("\n".join(lines) + "\n", encoding="utf-8")

    findings = {family.profiles[s]: list(v) for s, v in FINDINGS.items()}
    genome.list_findings = lambda conn, pid, **kw: genome.Findings(findings=[dict(f) for f in findings.get(pid, [])])
    genome.pgx_for_drug = lambda conn, pid, **kw: [dict(f) for f in findings.get(pid, []) if f["category"] == "pgx"]

    pack = load_pack()
    out: dict = {"profiles": {s: str(family.profiles[s]) for s in SLUGS}, "users": {k: str(v) for k, v in family.users.items()}}
    out["pack"] = {"version": pack.version, "sha256": pack.sha256}
    keep = ("id", "kind", "file", "system", "match", "window_days", "cite", "basis", "note", "verify")
    out["rules"] = {
        "version": pack.version,
        "sha256": pack.sha256,
        "systems": [{"key": s.key, "file": s.file, "name": s.names, "source": s.source} for s in pack.systems],
        "rules": [{k: r[k] for k in keep if k in r} for r in pack.rules],
        "cpic": {k: cpic_snapshot()[k] for k in ("source", "license", "retrieved")},
    }
    with eng.connect() as conn:
        tx = conn.begin()
        bodies, renders, dashboards, drugs = {}, {}, {}, {}
        for slug in SLUGS:
            pid = family.profiles[slug]
            body = state.build_state(conn, pid, as_of=AS_OF)
            bodies[slug] = body
            for lang in ("en", "es"):
                for mode in ("full", "pseudonymous", "minimal"):
                    renders[f"{slug}/{lang}/{mode}"] = state.render(body, lang=lang, identity_mode=mode)
                for units in ("si", "conventional"):
                    renders[f"{slug}/{lang}/units:{units}"] = state.render(body, lang=lang, units=units)
                d = state.dashboard(conn, pid, as_of=AS_OF, lang=lang, scopes=ALL_SCOPES)
                d.pop("generated_at")
                dashboards[f"{slug}/{lang}"] = d
        body = bodies["alex"]
        renders["alex/summary"] = state.render(body, lang="en", scopes=frozenset({"read:summary"}))
        renders["alex/no_genome"] = state.render(body, lang="en", scopes=ALL_SCOPES - {"read:genome"})
        renders["alex/all"] = state.render(body, lang="en", scopes=ALL_SCOPES)
        d = state.dashboard(conn, family.profiles["alex"], as_of=AS_OF, lang="en", scopes=frozenset({"read:summary"}))
        d.pop("generated_at")
        dashboards["alex/summary"] = d
        bodies["alex@2025-02-01"] = state.build_state(conn, family.profiles["alex"], as_of=date(2025, 2, 1))
        for slug in ("alex", "sam"):
            for drug in sorted(pack.drugs):
                for lang in ("en", "es"):
                    drugs[f"{slug}/{drug}/{lang}"] = state.check_drug(
                        conn, family.profiles[slug], drug, scopes=ALL_SCOPES, lang=lang, as_of=AS_OF
                    )
        for scope in ("read:labs", "read:genome", "read:records"):
            drugs[f"alex/clopidogrel/{scope}"] = state.check_drug(
                conn, family.profiles["alex"], "clopidogrel", scopes=frozenset({scope}), as_of=AS_OF
            )
        words = {
            g: state.status_word(conn, family.profiles["alex"], g, as_of=AS_OF)
            for g in ("vitamin_d_25oh", "tsh", "crp_hs", "ldl_c", "hba1c", "lpa", "hcv_ab", "glucose", "unknown_x")
        }
        tx.rollback()
    out |= {"bodies": bodies, "renders": renders, "dashboards": dashboards, "check_drug": drugs, "status_words": words}
    (HERE / "expected.json").write_text(json.dumps(out, ensure_ascii=False, indent=1) + "\n", encoding="utf-8")
    eng.dispose()


if __name__ == "__main__":
    main()
