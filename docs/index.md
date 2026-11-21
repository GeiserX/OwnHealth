---
hide:
  - navigation
---

# OwnHealth { .oh-visually-hidden }

<!-- Home proposal A, product landing. The picked banner goes above the hero. -->

<div class="oh-hero" markdown>
<div markdown>
<p class="oh-hero__title">The health data layer you own</p>

Labs, records, wearables and genome in one self-hosted store, with a web dashboard and an MCP server for your own agent.

<span class="oh-status">Pre-alpha · 0.4.0 · AGPL-3.0-or-later</span>

[Quick start](quickstart.md){ .md-button .md-button--primary }
[MCP tool reference](mcp-tools.md){ .md-button }
</div>
<div markdown>
![Health at a glance: one card per body system for one person of the synthetic Rivera family, in the dark theme](mockup/screenshots/01-dashboard.png)
</div>
</div>

<p align="center">
  <a href="https://github.com/GeiserX/OwnHealth/blob/main/CHANGELOG.md"><img alt="Version" src="https://img.shields.io/github/v/tag/GeiserX/OwnHealth?style=flat-square&label=version"></a>
  <a href="https://hub.docker.com/r/drumsergio/ownhealth"><img alt="Docker Pulls" src="https://img.shields.io/docker/pulls/drumsergio/ownhealth?style=flat-square&logo=docker"></a>
  <a href="https://github.com/GeiserX/OwnHealth/stargazers"><img alt="GitHub Stars" src="https://img.shields.io/github/stars/GeiserX/OwnHealth?style=flat-square&logo=github"></a>
  <a href="https://github.com/GeiserX/OwnHealth/blob/main/LICENSE"><img alt="License: AGPL-3.0-or-later" src="https://img.shields.io/github/license/GeiserX/OwnHealth?style=flat-square"></a>
</p>

---

## Why it exists

Your health data sits in lab portals, PDFs, phone apps and a genome file, often from more than one country. OwnHealth puts it in one store on a machine you run. Each value stays linked to the page it came from. Your own agent can read the store over MCP, and every answer cites its source.

There is no AI model inside OwnHealth and no account with anyone. The agent is yours, and it sees only what its token allows.

## What it does

- One profile per person, and one dashboard for the household. Each body system shows OK, Watch, Out, Stale or No data.
- Reads lab PDFs into values you can trend. A value it is not sure of waits in a [review queue](quickstart.md#3-import-a-lab-report).
- Takes data from a mailbox, a watched folder, Paperless-ngx, a phone, FHIR and IPS bundles, Apple Health, Gadgetbridge and Nightscout. The [recipes](recipes/README.md) set up each one.
- Shows genome findings from [OwnGenome](https://github.com/GeiserX/OwnGenome) next to your medicines and labs, and quotes the CPIC guideline for a drug.
- Runs published [calculators](calculators/README.md) (eGFR, FIB-4, HOMA-IR, SCORE2, PREVENT) with their citations, or says why one does not apply.
- Serves an [MCP server](mcp-tools.md) with 10 tools, scoped per person and per token.

## What it looks like

These screens come from the [design mockup](https://github.com/GeiserX/OwnHealth/tree/main/docs/mockup), not from the running app. They show the synthetic Rivera family, and some show parts planned for later releases.

<div class="oh-gallery" markdown>
<figure markdown>
![LDL cholesterol from 2019 to 2026 across four labs, each lab's printed range, and every measurement with its source](mockup/screenshots/02-biomarker.png)
<figcaption>A marker over the years</figcaption>
</figure>
<figure markdown>
![A lab page with the read values boxed, and the extracted rows waiting for review](mockup/screenshots/08-import-review.png)
<figcaption>Import and review</figcaption>
</figure>
<figure markdown>
![A drug check that quotes the CPIC guideline for a CYP2C19 genotype, next to the current medicines](mockup/screenshots/05-drug-check.png)
<figcaption>Drug check</figcaption>
</figure>
<figure markdown>
![The household: members, roles and the grants between them](mockup/screenshots/09-family.png)
<figcaption>The family</figcaption>
</figure>
<figure markdown>
![Tokens with their scopes, the claude mcp add command and the audit log](mockup/screenshots/10-mcp-access.png)
<figcaption>MCP and access</figcaption>
</figure>
<figure markdown>
![Visits, labs, medicines and vaccines on one timeline, filterable by country](mockup/screenshots/03-timeline.png)
<figcaption>Timeline</figcaption>
</figure>
</div>

## Start in ten minutes

You need Docker with Compose v2. Clone the repository, fill four secrets in `.env`, and run `docker compose up -d`. Open http://localhost:8080, register a passkey, and drop a lab report PDF on **Import & review**. The [quick start](quickstart.md) has every command, and a demo family to look around with first.

## What it does not do

- It does not diagnose. It keeps records and shows values with the formulas and sources they come from.
- It does not tell anyone what to take or stop. A pharmacogenomic result quotes the published guideline and links it.
- It sends nothing anywhere. The app listens on `127.0.0.1` until you put a reverse proxy in front.

## Privacy and security

Logins use passkeys. Every read through MCP goes to an append-only audit log. A token reaches only the people and the kinds of data you pick, and free text from a document reaches an agent marked as untrusted. [Security, privacy and regulation](design/security-privacy.md) says what we protect, and from whom.

## Every page

- **Get started:** [Quick start](quickstart.md), [Recipes](recipes/README.md) ([email](recipes/email.md), [Paperless-ngx](recipes/paperless-ngx.md), [scanner to folder](recipes/scanner-to-folder.md), [Health Auto Export](recipes/health-auto-export.md), [Gadgetbridge](recipes/gadgetbridge.md), [Nightscout](recipes/nightscout.md), [iOS Shortcut](recipes/ios-shortcut.md), [claude.ai connector](recipes/claude-connector.md)), [Agent-assisted extraction](agent-extraction.md).
- **Reference:** [MCP tools](mcp-tools.md), [Calculators](calculators/README.md) ([eGFR](calculators/ckd-epi-2021.md), [FIB-4](calculators/fib-4.md), [HOMA-IR](calculators/homa-ir.md), [SCORE2](calculators/score2.md), [PREVENT](calculators/prevent.md), [RCV](calculators/rcv.md), [baseline](calculators/baseline.md)), [Guideline files](guidelines.md), [OCR benchmark](ocr-benchmark.md).
- **How it works:** [architecture](design/architecture.md), [ingestion](design/ingestion.md), [connectors](design/connectors.md), [MCP design](design/mcp.md), [genome data](design/genomics.md), [security and privacy](design/security-privacy.md).

## Getting help

- Something broken: open an [issue](https://github.com/GeiserX/OwnHealth/issues) with synthetic data only.
- A security problem: follow the [security policy](https://github.com/GeiserX/OwnHealth/blob/main/SECURITY.md), never a public issue.
- What changed: the [changelog](https://github.com/GeiserX/OwnHealth/blob/main/CHANGELOG.md). What comes next: the [roadmap](https://github.com/GeiserX/OwnHealth/blob/main/docs/ROADMAP.md).

## License

OwnHealth is released under the [AGPL-3.0-or-later](https://github.com/GeiserX/OwnHealth/blob/main/LICENSE) license.
