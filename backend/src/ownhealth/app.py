"""FastAPI app factory: REST routers under /api, the MCP server at /mcp, the static UI at /."""

from __future__ import annotations

import asyncio
import importlib
import logging
import pkgutil
from contextlib import asynccontextmanager
from pathlib import Path

from fastapi import FastAPI, Request
from fastapi.responses import FileResponse, JSONResponse
from mcp.server.transport_security import TransportSecuritySettings
from sqlalchemy import Engine, text

from ownhealth import __version__, runtime
from ownhealth.config import Settings
from ownhealth.core.access import AccessDenied
from ownhealth.core.lifecycle import LifecycleError
from ownhealth.core.provenance import ProvenanceError
from ownhealth.core.review import ReviewError

log = logging.getLogger("ownhealth")


def discover_routers() -> list:
    """Every ownhealth.api.<module> with a `router` attribute, in module-name order."""
    from ownhealth import api

    routers = []
    for mod in sorted(pkgutil.iter_modules(api.__path__), key=lambda m: m.name):
        if mod.name.startswith("_"):
            continue
        module = importlib.import_module(f"{api.__name__}.{mod.name}")
        router = getattr(module, "router", None)
        if router is not None:
            routers.append((mod.name, router))
    return routers


def _error(status: int, code: str, message: str, **extra) -> JSONResponse:
    return JSONResponse(status_code=status, content={"detail": {"code": code, "message": message, **extra}})


def create_app(settings: Settings | None = None, engine: Engine | None = None) -> FastAPI:
    settings = settings or Settings.from_env()
    rt = runtime.init(settings, engine)

    from ownhealth.mcp import build_server

    mcp_server = build_server(settings)
    mcp_app = mcp_server.streamable_http_app(
        streamable_http_path="/mcp",
        stateless_http=True,
        json_response=True,
        # Origin checks against OWNHEALTH_BASE_URL are lane F's (mcp.md section 1).
        transport_security=TransportSecuritySettings(enable_dns_rebinding_protection=False),
    )

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        if settings.run_migrations:
            from ownhealth.db.migrate import migrate

            applied = await asyncio.to_thread(migrate, rt.engine)
            if applied:
                log.info("applied migrations %s", applied)
        if settings.demo:
            from ownhealth.demo import seed_demo

            await asyncio.to_thread(seed_demo, rt.engine, settings, rt.blobs)
        stop = asyncio.Event()
        tasks = []
        if settings.run_jobs:
            from ownhealth.jobs import JobContext, start_workers

            tasks = start_workers(JobContext(rt.engine, settings, rt.blobs), stop)
        async with mcp_server.session_manager.run():
            yield
        stop.set()
        for t in tasks:
            t.cancel()
        await asyncio.gather(*tasks, return_exceptions=True)

    app = FastAPI(
        title="OwnHealth",
        version=__version__,
        lifespan=lifespan,
        docs_url="/api/docs",
        openapi_url="/api/openapi.json",
        redoc_url=None,
    )
    app.state.runtime = rt
    app.state.mcp = mcp_server

    @app.exception_handler(AccessDenied)
    async def _denied(request: Request, exc: AccessDenied):
        status = 404 if exc.code == "profile_not_granted" else 403
        return _error(status, exc.code, str(exc), allowed=exc.allowed)

    @app.exception_handler(ReviewError)
    @app.exception_handler(LifecycleError)
    @app.exception_handler(ProvenanceError)
    async def _conflict(request: Request, exc: Exception):
        return _error(409, "conflict", str(exc))

    @app.get("/healthz", include_in_schema=False)
    def healthz() -> dict:
        return {"status": "ok", "version": __version__}

    @app.get("/readyz", include_in_schema=False)
    def readyz():
        from ownhealth.db.migrate import pending

        try:
            with rt.engine.connect() as conn:
                conn.execute(text("select 1"))
            todo = pending(rt.engine)
        except Exception as exc:  # noqa: BLE001
            return _error(503, "db_unavailable", type(exc).__name__)
        if todo:
            return _error(503, "migrations_pending", f"pending: {todo}")
        if not rt.blobs.root.is_dir():
            return _error(503, "blob_dir_missing", str(rt.blobs.root))
        return {"status": "ready"}

    for name, router in discover_routers():
        app.include_router(router, prefix="/api")
        log.debug("mounted api/%s", name)

    app.router.routes.extend(mcp_app.routes)

    _mount_ui(app, settings.static_dir)
    return app


def _mount_ui(app: FastAPI, static_dir: Path) -> None:
    """Serve the built SvelteKit app; unknown non-API paths fall back to index.html (SPA)."""
    root = Path(static_dir).resolve()

    @app.get("/{path:path}", include_in_schema=False)
    def ui(path: str):
        if path in ("api", "mcp") or path.startswith(("api/", "mcp/")):
            return _error(404, "not_found", "Not found.")
        candidate = (root / path).resolve()
        if path and candidate.is_file() and candidate.is_relative_to(root):
            return FileResponse(candidate)
        index = root / "index.html"
        if index.is_file():
            return FileResponse(index, headers={"Cache-Control": "no-cache"})
        return _error(503, "ui_not_built", "The UI is not built. Run `pnpm build` in ui/.")
