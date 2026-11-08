import shutil

import pytest
from sqlalchemy import text

from ownhealth.db import make_engine
from ownhealth.db.migrate import MIGRATIONS_DIR, MigrationError, discover, migrate, pending


def test_session_db_is_fully_migrated(engine):
    assert pending(engine) == []
    with engine.connect() as c:
        versions = c.execute(text("select version from schema_migrations order by version")).scalars().all()
    assert versions[0] == 1
    assert migrate(engine) == []  # idempotent


def test_migration_files_are_numbered_and_unique():
    versions = [m.version for m in discover()]
    assert versions == sorted(set(versions))


def test_refuses_schema_newer_than_code_and_edited_migrations(make_database, tmp_path):
    url = make_database()
    eng = make_engine(url)
    try:
        newer = tmp_path / "newer"
        shutil.copytree(MIGRATIONS_DIR, newer)
        (newer / "9999_future.sql").write_text("create table future_thing (id int);")
        assert 9999 in migrate(eng, newer)
        with pytest.raises(MigrationError, match="newer than the code"):
            migrate(eng, MIGRATIONS_DIR)

        edited = tmp_path / "edited"
        shutil.copytree(newer, edited)
        (edited / "9999_future.sql").write_text("create table future_thing (id bigint);")
        with pytest.raises(MigrationError, match="forward-only"):
            migrate(eng, edited)
    finally:
        eng.dispose()


def test_bad_file_name_is_rejected(tmp_path):
    (tmp_path / "1_bad.sql").write_text("select 1;")
    with pytest.raises(MigrationError):
        discover(tmp_path)
