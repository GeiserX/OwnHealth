# syntax=docker/dockerfile:1
# OwnHealth: one app image (FastAPI + MCP + built UI). Postgres runs beside it (compose.yaml).

# 1. UI: SvelteKit static build, written into the Python package's static/ folder.
FROM node:24.22.2-bookworm-slim AS ui
WORKDIR /src/ui
RUN corepack enable
COPY ui/package.json ui/pnpm-lock.yaml ui/.npmrc ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN mkdir -p ../backend/src/ownhealth/static && pnpm build

# 2. Python: locked dependencies and the app into /app/.venv, non-editable.
FROM python:3.13.18-slim-trixie AS build
ENV UV_COMPILE_BYTECODE=1 UV_LINK_MODE=copy UV_PYTHON_DOWNLOADS=never
RUN pip install --no-cache-dir uv==0.11.29
WORKDIR /app
COPY backend/pyproject.toml backend/uv.lock ./
RUN uv sync --frozen --no-dev --no-install-project
COPY backend/src ./src
COPY --from=ui /src/backend/src/ownhealth/static ./src/ownhealth/static
RUN uv sync --frozen --no-dev --no-editable \
 && /app/.venv/bin/python -c "import pysam, pdfplumber, pypdfium2, webauthn, ownhealth"

# 3. Runtime: slim, non-root, no build tools.
FROM python:3.13.18-slim-trixie
LABEL org.opencontainers.image.title="OwnHealth" \
      org.opencontainers.image.source="https://github.com/GeiserX/OwnHealth" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later"
RUN groupadd --system --gid 10001 ownhealth \
 && useradd --system --uid 10001 --gid ownhealth --home-dir /data --shell /usr/sbin/nologin ownhealth \
 && mkdir -p /data && chown ownhealth:ownhealth /data
COPY --from=build /app/.venv /app/.venv
ENV PATH="/app/.venv/bin:$PATH" \
    PYTHONUNBUFFERED=1 \
    OWNHEALTH_DATA_DIR=/data \
    OWNHEALTH_HOST=0.0.0.0 \
    OWNHEALTH_PORT=8080
USER ownhealth
WORKDIR /data
VOLUME ["/data"]
EXPOSE 8080
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["ownhealth", "healthcheck"]
CMD ["ownhealth", "serve"]
