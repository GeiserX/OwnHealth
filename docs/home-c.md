---
hide:
  - navigation
---

# OwnHealth { .oh-visually-hidden }

<!-- Home proposal C, you and your agent. The picked banner goes above the first line. -->

<p class="oh-hero__title">One health record, read two ways</p>

OwnHealth keeps your family's labs, records, wearables and genome findings in one store on a machine you run. You read it in the browser. Your own agent reads it over MCP. Both see the same values, and every value names the page it came from.

<span class="oh-status">Pre-alpha · 0.4.0 · information, never a diagnosis</span>

<div class="oh-split" markdown>
<div markdown>

### You, in the browser

![Health at a glance for one person of the synthetic Rivera family](mockup/screenshots/01-dashboard.png){ .oh-shot }

- One profile per person, with OK, Watch, Out, Stale or No data per body system.
- Every value opens the page and the box it was read from.
- Anything the importer is not sure of waits for you in a review queue.

[Quick start](quickstart.md){ .md-button .md-button--primary }

</div>
<div markdown>

### Your agent, over MCP

```sh
claude mcp add --transport http ownhealth \
  http://localhost:8080/mcp \
  --header "Authorization: Bearer ${OWNHEALTH_PAT}"
```

Ask "how is my health right now?" and the agent calls `get_health_status`:

```json
{"rank": 1,
 "kind": "out_of_range",
 "rule": "range.lab",
 "code": "LOINC:43583-4",
 "text": "Lp(a) 142 nmol/L (H, lab range < 75) on 2025-09-08.",
 "src": "src_a07"}
```

- The token picks the people and the kinds of data; the agent sees nothing else.
- `get_source` turns any `src` back into the document and page.
- Free text from a document arrives marked as untrusted, never as instructions.

[MCP tool reference](mcp-tools.md){ .md-button }

</div>
</div>

## A week with the Rivera family

The household is invented: Alex and Sam Rivera, their daughter Lucía (9) and Carmen Ortega (71), a parent. Every value below is synthetic, and the screens come from the [design mockup](https://github.com/GeiserX/OwnHealth/tree/main/docs/mockup).

1. **A lab report arrives.** The lab mails Alex's results to the household's own mailbox. The sender is allowed, so the PDF goes to Alex's profile ([Email recipe](recipes/email.md)). Mail from a sender nobody allowed waits in quarantine, unread.

    ![A lab page with the read values boxed, and the extracted rows waiting for review](mockup/screenshots/08-import-review.png){ .oh-shot }

2. **Alex reviews what was not sure.** The values that passed every check are saved at once. The rest wait beside the page, each in its box. Alex accepts, fixes or rejects each row.

3. **The dashboard updates.** Lp(a) shows Out against the lab's own printed range. TSH shows Stale: the newest one is over a year old. A hepatitis C antibody test was never measured, so it shows as a test to ask for.

4. **Alex asks the agent.** Claude Code answers from `get_health_status` and cites `src_a07` for the Lp(a) value. The answer says what it could not assess, and that it is information, not a diagnosis.

5. **A new prescription.** Alex's genome findings come from [OwnGenome](https://github.com/GeiserX/OwnGenome). The drug check for clopidogrel quotes the CPIC guideline for CYP2C19 \*1/\*2 next to the current medicines, and ends with "discuss with your prescriber".

    ![A drug check that quotes the CPIC guideline for a CYP2C19 genotype, next to the current medicines](mockup/screenshots/05-drug-check.png){ .oh-shot }

6. **The rest of the family.** Each member has a role on each profile: owner, guardian, caregiver or viewer. A role opens only what its grant allows, and each person can lock a category.

## What it never does

- It runs no AI model and calls none. The agent is yours.
- It gives no diagnosis and tells nobody what to take or stop.
- It sends nothing anywhere. The app listens on `127.0.0.1` until you put a reverse proxy in front. [Security, privacy and regulation](design/security-privacy.md) has the threat model.

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
