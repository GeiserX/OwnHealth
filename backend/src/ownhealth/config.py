"""Settings from OWNHEALTH_* environment variables.

Every variable has a `*_FILE` twin for secrets mounted as files (Docker secrets):
`OWNHEALTH_SECRET_KEY_FILE=/run/secrets/secret_key` wins over `OWNHEALTH_SECRET_KEY`.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path
from urllib.parse import quote

PACKAGE_DIR = Path(__file__).resolve().parent


def _env(name: str, default: str | None = None) -> str | None:
    file_path = os.environ.get(f"OWNHEALTH_{name}_FILE")
    if file_path:
        return Path(file_path).read_text().strip()
    return os.environ.get(f"OWNHEALTH_{name}", default)


def _flag(name: str, default: bool = False) -> bool:
    value = _env(name)
    if value is None:
        return default
    return value.strip().lower() in {"1", "true", "yes", "on"}


def sqlalchemy_url(url: str) -> str:
    """Point a plain postgresql:// URL at the psycopg 3 driver."""
    for prefix in ("postgresql+psycopg://", "postgresql://", "postgres://"):
        if url.startswith(prefix):
            return "postgresql+psycopg://" + url[len(prefix) :]
    return url


DEV_SECRET_KEY = "dev-insecure-secret-change-me"  # public: create_app refuses it outside dev_auth and demo


@dataclass(frozen=True)
class Settings:
    database_url: str = "postgresql://ownhealth:ownhealth@127.0.0.1:5432/ownhealth"
    data_dir: Path = Path("./data")
    blob_dir: Path | None = None
    static_dir: Path = PACKAGE_DIR / "static"
    base_url: str = "http://127.0.0.1:8080"
    default_locale: str = "es"
    secret_key: str = DEV_SECRET_KEY
    demo: bool = False
    # Unauthenticated requests act as the first owner user. Never on a real install.
    dev_auth: bool = False
    run_migrations: bool = True
    run_jobs: bool = True
    job_workers: int = 2
    job_poll_seconds: float = 2.0
    queues: tuple[str, ...] = ("core",)
    auto_accept_threshold: float = 0.95
    age_of_majority: int = 18
    host: str = "0.0.0.0"
    port: int = 8080
    extra: dict[str, str] = field(default_factory=dict)

    @property
    def blobs_path(self) -> Path:
        return self.blob_dir or (self.data_dir / "blobs")

    @property
    def sqlalchemy_url(self) -> str:
        return sqlalchemy_url(self.database_url)

    @classmethod
    def from_env(cls) -> Settings:
        d = cls()
        url = _env("DATABASE_URL") or os.environ.get("DATABASE_URL") or d.database_url
        password = _env("DB_PASSWORD")
        if password and "@" in url and ":" not in url.split("://", 1)[1].split("@", 1)[0]:
            scheme, rest = url.split("://", 1)
            user, host = rest.split("@", 1)
            url = f"{scheme}://{user}:{quote(password, safe='')}@{host}"
        data_dir = Path(_env("DATA_DIR") or str(d.data_dir))
        blob_dir = _env("BLOB_DIR")
        return cls(
            database_url=url,
            data_dir=data_dir,
            blob_dir=Path(blob_dir) if blob_dir else None,
            static_dir=Path(_env("STATIC_DIR") or str(d.static_dir)),
            base_url=(_env("BASE_URL") or _env("ORIGIN") or d.base_url).rstrip("/"),
            default_locale=_env("DEFAULT_LOCALE") or d.default_locale,
            secret_key=_env("SECRET_KEY") or d.secret_key,
            demo=_flag("DEMO"),
            dev_auth=_flag("DEV_AUTH"),
            run_migrations=_flag("RUN_MIGRATIONS", True),
            run_jobs=_flag("RUN_JOBS", True),
            job_workers=int(_env("JOB_WORKERS") or d.job_workers),
            job_poll_seconds=float(_env("JOB_POLL_SECONDS") or d.job_poll_seconds),
            queues=tuple((_env("QUEUES") or "core").split(",")),
            auto_accept_threshold=max(0.90, float(_env("AUTO_ACCEPT_THRESHOLD") or d.auto_accept_threshold)),
            age_of_majority=int(_env("AGE_OF_MAJORITY") or d.age_of_majority),
            host=_env("HOST") or d.host,
            port=int(_env("PORT") or d.port),
        )
