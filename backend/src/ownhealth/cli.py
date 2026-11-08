"""`ownhealth serve | migrate | demo-seed | worker | healthcheck | backup | restore | verify-restore`."""

from __future__ import annotations

import argparse
import asyncio
import logging
import sys
import urllib.request

from ownhealth.config import Settings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="ownhealth")
    sub = parser.add_subparsers(dest="cmd", required=True)
    sub.add_parser("serve", help="run the web app, MCP server and job loop")
    sub.add_parser("migrate", help="apply pending database migrations")
    seed = sub.add_parser("demo-seed", help="load the synthetic Rivera family")
    seed.add_argument("--force", action="store_true", help="seed even if the demo household exists")
    sub.add_parser("worker", help="run only the job loop (OWNHEALTH_QUEUES)")
    sub.add_parser("healthcheck", help="exit 0 when /healthz answers")
    for name, what in (
        ("backup", "write a consistent backup directory (database dump, blobs, manifest) for restic"),
        ("restore", "load a backup directory into this empty instance and check every state_hash"),
        ("verify-restore", "restore a backup into a scratch database, compare with its manifest, drop it"),
    ):
        sub.add_parser(name, help=what).add_argument("path", help="the backup directory")
    args = parser.parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s %(message)s")
    settings = Settings.from_env()

    if args.cmd == "serve":
        import uvicorn

        from ownhealth.app import create_app

        uvicorn.run(
            create_app(settings),
            host=settings.host,
            port=settings.port,
            workers=1,
            proxy_headers=True,
            log_level="info",
        )
        return 0

    if args.cmd in ("backup", "restore", "verify-restore"):
        import json
        from pathlib import Path

        from ownhealth import ops

        try:
            if args.cmd == "backup":
                print(ops.backup(settings, Path(args.path)))
                return 0
            fn = ops.restore if args.cmd == "restore" else ops.verify_restore
            report = fn(settings, Path(args.path))
        except ops.OpsError as exc:
            print(f"error: {exc}", file=sys.stderr)
            return 1
        print(json.dumps(report, indent=2))
        return 0 if report["ok"] else 1

    from ownhealth import runtime

    rt = runtime.init(settings)
    if args.cmd == "migrate":
        from ownhealth.db.migrate import migrate

        print("applied:", migrate(rt.engine) or "nothing")
        return 0
    if args.cmd == "demo-seed":
        from ownhealth.db.migrate import migrate
        from ownhealth.demo import seed_demo

        migrate(rt.engine)
        done = seed_demo(rt.engine, settings, rt.blobs, force=args.force)
        print("demo household seeded" if done else "demo household exists; use --force to add another")
        return 0
    if args.cmd == "worker":
        from ownhealth.jobs import JobContext, start_workers

        async def run() -> None:
            stop = asyncio.Event()
            await asyncio.gather(*start_workers(JobContext(rt.engine, settings, rt.blobs), stop))

        asyncio.run(run())
        return 0
    if args.cmd == "healthcheck":
        try:
            with urllib.request.urlopen(f"http://127.0.0.1:{settings.port}/healthz", timeout=3) as r:
                return 0 if r.status == 200 else 1
        except OSError:
            return 1
    return 2


if __name__ == "__main__":
    sys.exit(main())
