# AGENTS.md

OwnHealth is a self-hosted personal health record: a web app that keeps a person's (and a family's) labs, records, wearables, genome and imaging in one place, and an MCP server that lets an outside agent read it. Pre-alpha: v0.1 is being built; the plan lives in `docs/`, and `CONTRACTS.md` says who owns which file and which interface.

## Rules for agents

- The app has no built-in chat agent. The MCP server is the AI interface; keep it that way.
- Never commit real health data, names, genomes or lab reports. Fixtures are synthetic: Alex, Sam and Lucía Rivera and Carmen Ortega, and the fictional lab LabNorte. No real lab brands.
- Every stored value keeps its provenance: source document, page and position.
- Dark theme by default; Spanish and English from the first screen.
- `docs/mockup/index.html` is the visual spec for the UI; the real UI must match it (palette, type, cards, chips, sparklines, layout, copy, es/en).
- Information, not diagnosis: no imperative medical advice. Status words are OK, Watch, Out, Stale and No data.
- Free text sent over MCP is wrapped as `untrusted_text`.
- Database changes are new numbered SQL files in your lane's range (`CONTRACTS.md` section 7). Never edit an applied migration.
- Conventional commits. Every PR runs CI and is never opened as a draft.
- License: AGPL-3.0-or-later. Every package manifest uses that SPDX id.

## Dev commands

Backend (Python 3.13, uv), from `backend/`:

```sh
../scripts/test-db.sh           # shared test Postgres 17 on 127.0.0.1:55432 (idempotent)
uv sync                         # install
uv run pytest -q                # each session creates and drops its own database
uv run ruff check src tests && uv run ruff format --check src tests
uv run ownhealth migrate        # apply migrations to OWNHEALTH_DATABASE_URL
uv run ownhealth demo-seed      # seed the synthetic Rivera family (--force to add another)
OWNHEALTH_DEV_AUTH=1 OWNHEALTH_DEMO=1 uv run ownhealth serve   # http://127.0.0.1:8080, MCP at /mcp
```

UI (SvelteKit, pnpm), from `ui/`:

```sh
pnpm install
pnpm dev                        # proxies /api and /mcp to OWNHEALTH_BACKEND (default 127.0.0.1:8080)
pnpm run check                  # svelte-check, warnings fail
pnpm build                      # writes backend/src/ownhealth/static, served by FastAPI
```

Container: `cp .env.example .env`, fill it in, `docker compose up -d`. The image is `drumsergio/ownhealth`; Postgres is pinned to a full 17.x version.
