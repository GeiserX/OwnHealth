"""Measure peak RSS of the Apple Health import on a large synthetic export (not a pytest test).

    uv run python tests/imports/measure_memory.py --days 400            # parse only
    uv run python tests/imports/measure_memory.py --days 400 --db       # full import into a scratch DB

Each mode runs in its own process so the peak RSS is that mode's alone. The scratch database is
created on the shared test Postgres and dropped at the end. The numbers go in
fixtures/apple_health/README.md.
"""

from __future__ import annotations

import argparse
import resource
import secrets
import sys
import tempfile
import time
import zipfile
from datetime import date
from pathlib import Path

from ownhealth.ingest.apple_health.parse import ExportReader
from ownhealth.ingest.apple_health.synthetic import ExportSpec, write_export

ADMIN = "postgresql://postgres:postgres@127.0.0.1:55432/postgres"


def rss_mb() -> float:
    peak = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    return peak / (1024 * 1024) if sys.platform == "darwin" else peak / 1024


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--days", type=int, default=400)
    ap.add_argument("--db", action="store_true")
    args = ap.parse_args()
    tmp = Path(tempfile.mkdtemp(prefix="oh-mem-"))
    path = write_export(
        tmp / "export.zip",
        ExportSpec(days=args.days, hr_every_min=1, end=date(2026, 11, 7), birth_date=date(1979, 5, 14)),
    )
    with zipfile.ZipFile(path) as z:
        xml_mb = z.getinfo("apple_health_export/export.xml").file_size / 1e6
    base = rss_mb()
    t0 = time.monotonic()
    if not args.db:
        with zipfile.ZipFile(path) as z, z.open("apple_health_export/export.xml") as f:
            n = sum(1 for _ in ExportReader(f).samples())
    else:
        from sqlalchemy import text

        from ownhealth.blobs import BlobStore
        from ownhealth.core.documents import create_document
        from ownhealth.db import make_engine
        from ownhealth.db.migrate import migrate
        from ownhealth.demo.family import create_rivera_family
        from ownhealth.ingest.apple_health import import_apple_health

        admin = make_engine(ADMIN, isolation_level="AUTOCOMMIT")
        name = f"oh_mem_{secrets.token_hex(4)}"
        with admin.connect() as c:
            c.execute(text(f'create database "{name}"'))
        try:
            eng = make_engine(ADMIN.rsplit("/", 1)[0] + "/" + name)
            migrate(eng)
            blobs = BlobStore(tmp / "blobs")
            sha, size = blobs.put_file(path)
            with eng.begin() as c:
                fam = create_rivera_family(c, "Rivera mem")
                doc = create_document(
                    c,
                    household_id=fam.household_id,
                    profile_id=fam.profiles["alex"],
                    blob_sha256=sha,
                    mime="application/zip",
                    size_bytes=size,
                    filename="export.zip",
                )
                n = import_apple_health(c, blobs, document_id=doc.id).accepted
            eng.dispose()
        finally:
            with admin.connect() as c:
                c.execute(text(f'drop database if exists "{name}" with (force)'))
    took = time.monotonic() - t0
    print(
        f"mode={'db' if args.db else 'parse'} export.xml={xml_mb:.0f} MB samples={n} seconds={took:.1f}"
        f" rss_before={base:.0f} MB peak_rss={rss_mb():.0f} MB"
    )
    for p in sorted(tmp.rglob("*"), reverse=True):
        p.unlink() if p.is_file() else p.rmdir()
    tmp.rmdir()


if __name__ == "__main__":
    main()
