# Changelog

## 0.1.0 (2026-11-08)

The first release you can run: `docker compose up` with one app image and Postgres 17.

- **Imports.** Lab PDFs with a text layer (rows read with page and position, coded to LOINC, units converted, checked against the printed range, auto-accepted only when every check passes, else the review queue). Apple Health `export.zip`, FHIR R4 and IPS bundles, manual entry, and OwnGenome `genome-facts.json` with its VCF. Scans and photos wait as pending documents for an agent to read over MCP.
- **Status.** One card per body system (OK, Watch, Out, Stale, No data) from a cited YAML rule pack, with a reproducible `state_hash`. Drug check with CPIC level A quotes, current medications, relevant labs and phenoconversion.
- **MCP** at `/mcp`: `get_health_status`, `search_observations`, `get_trend`, `get_genome_findings`, `lookup_variant`, `check_drug`, `get_source`, `propose`. Hashed scoped PATs, scope-filtered `tools/list`, `untrusted_text` on free text, agent writes only into the review queue, rate limits, audit log.
- **Security.** Passkeys with recovery codes, step-up, household roles, full export (FHIR NDJSON plus originals) from the UI only.
- **UI.** Dark theme, Spanish and English, built to match the mockup.

Known limits: no curated rsID index yet, so rsID lookups outside your own facts file answer `rsid_unknown`; lab coverage is the synthetic eval set; no OCR (v0.2). Measured exit criteria: [docs/v0.1-exit-criteria.md](docs/v0.1-exit-criteria.md).
