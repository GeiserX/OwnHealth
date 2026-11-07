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
