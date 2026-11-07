# AGENTS.md

OwnHealth is a self-hosted personal health record: a web app that keeps a person's (and a family's) labs, records, wearables, genome and imaging in one place, and an MCP server that lets an outside agent read it. Pre-alpha: the plan lives in `docs/`, there is no code yet.

## Rules for agents

- The app has no built-in chat agent. The MCP server is the AI interface; keep it that way.
- Never commit real health data, names, genomes or lab reports. Fixtures are synthetic.
- Every stored value keeps its provenance: source document, page and position.
- Dark theme by default; Spanish and English from the first screen.
- Conventional commits. Every PR runs CI and is never opened as a draft.
- License: AGPL-3.0-or-later. Every package manifest uses that SPDX id.
