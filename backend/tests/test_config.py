from sqlalchemy.engine import make_url

from ownhealth.config import Settings


def test_db_password_is_url_quoted(monkeypatch):
    monkeypatch.setenv("OWNHEALTH_DATABASE_URL", "postgresql://ownhealth@db:5432/ownhealth")
    monkeypatch.setenv("OWNHEALTH_DB_PASSWORD", "a/b+c=d@e:f")
    url = make_url(Settings.from_env().sqlalchemy_url)
    assert url.password == "a/b+c=d@e:f" and url.host == "db" and url.database == "ownhealth"


def test_file_twin_wins(monkeypatch, tmp_path):
    secret = tmp_path / "secret"
    secret.write_text("from-file\n")
    monkeypatch.setenv("OWNHEALTH_SECRET_KEY", "from-env")
    monkeypatch.setenv("OWNHEALTH_SECRET_KEY_FILE", str(secret))
    assert Settings.from_env().secret_key == "from-file"


def test_app_refuses_the_public_default_secret_key(settings, engine):
    import dataclasses

    import pytest

    from ownhealth.app import create_app
    from ownhealth.config import DEV_SECRET_KEY

    weak = dataclasses.replace(settings, secret_key=DEV_SECRET_KEY, dev_auth=False, demo=False)
    with pytest.raises(RuntimeError, match="OWNHEALTH_SECRET_KEY"):
        create_app(weak, engine)
    create_app(dataclasses.replace(weak, dev_auth=True), engine)  # a dev box may keep it


def test_queues_are_trimmed_and_unknown_names_refused(monkeypatch):
    import pytest

    monkeypatch.setenv("OWNHEALTH_QUEUES", "core, ocr,")
    assert Settings.from_env().queues == ("core", "ocr")
    monkeypatch.setenv("OWNHEALTH_QUEUES", "core,ocrr")
    with pytest.raises(ValueError, match="OWNHEALTH_QUEUES"):
        Settings.from_env()


def test_repr_hides_the_database_password_and_the_secret_key():
    s = Settings(database_url="postgresql://oh:pw-x1@db/oh", secret_key="sk-x2")
    assert "pw-x1" not in repr(s) and "sk-x2" not in repr(s)
