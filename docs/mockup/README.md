# OwnHealth web app mockup

A clickable mockup of the OwnHealth web page. Open `index.html` in a browser: it is one self-contained file (inline CSS, JS and SVG, no network), and it works from `file://`. Each screen has its own hash URL, for example `index.html#genome`. The ES / EN toggle in the top bar switches the navigation and the whole dashboard; `index.html?lang=es` opens in Spanish.

Every value is synthetic. The household is invented: Alex Rivera, Sam Rivera, Lucía Rivera (child, 9) and Carmen Ortega (parent, 71). The sidebar shows Alex's role on each profile (`owner`, `viewer`, `guardian`, `caregiver`). The profile switcher changes the header only; every profile shows Alex's values. Alex's values match the MCP examples in [PLAN.md](../PLAN.md#the-mcp-experience): Lp(a) 142 nmol/L high, vitamin D 24 ng/mL low, SLCO1B1 decreased function with atorvastatin, TSH stale, hepatitis C antibody never measured, CYP2C19 *1/*2 with omeprazole.

The phase chips (`v0.1`, `v0.2`, `v0.3`, `v1.0`) say in which [roadmap](../ROADMAP.md) phase a part ships. A screen chip covers the whole screen; a chip on a button, card or row overrides it for that element (on the dashboard: emergency card v1.0, doctor summary v0.3, screening and vaccine reminders v0.3).

Screenshots are 1440×900, dark colour scheme, taken with Playwright.

| # | Screen | URL | What it shows |
|---|---|---|---|
| 1 | Health at a glance | `#dashboard` | Per-system cards with status, key values, sparklines and stale badges; changes since the last draw; stale and never-measured items, then reminders; review count; current meds with PGx badges; genome highlights from OwnGenome; `state_hash` and rule pack version. |
| 2 | Biomarker detail | `#biomarker` | LDL-C 2019-2026 across 4 labs in 3 countries, each lab's printed range (no target band by default), statin bars, mg/dL ↔ mmol/L toggle, every measurement with LOINC and source page, PREVENT and SCORE2 with citations (v0.3). |
| 3 | Timeline | `#timeline` | Visits, labs, imaging, diagnoses, meds, vaccines and the genome ingest, filterable by type and by country (ES, DE, US), with sources and gaps. |
| 4 | Genome | `#genome` | Findings produced by [OwnGenome](https://github.com/GeiserX/OwnGenome) and imported from its `genome-facts.json`: summary tiles, findings with evidence E1-E4, confidence and `call_state` (`ref`, `alt`, `no_call`, `not_on_array`, `not_covered`), an APOE reveal gate, coverage and `not_assessed`, a ClinVar reinterpretation alert (v0.3). |
| 5 | Drug check | `#drug-check` | v0.1: clopidogrel against CYP2C19 *1/*2, the CPIC recommendations per indication with citation, omeprazole named as a CYP2C19 inhibitor (effect not computed: CPIC has no conversion rule for CYP2C19), current meds, relevant labs, drug-drug interactions marked not assessed, "discuss with your prescriber". |
| 6 | Wearables | `#wearables` | Sleep, HRV, resting HR, steps, VO₂max, CGM time in range, BP AM/PM, and per-device coverage. |
| 7 | Imaging | `#imaging` | Study list, a placeholder DICOM viewer with series thumbnails, the report as printed, and "Open in OHIF". |
| 8 | Import & review | `#import-review` | Inbox sources, a lab page with highlighted boxes, extracted rows with LOINC and confidence, an agent proposal made after `get_document_page` (v0.2), and a quarantined prompt-injection block. |
| 9 | Family | `#family` | Members and roles (`owner`, `guardian`, `caregiver`, `viewer`), a grants matrix, a pedigree with carrier flags, a cross-family carrier hint that names adults only, break-glass settings (v1.0). |
| 10 | MCP & access | `#mcp-access` | Tokens with scope chips, the `claude mcp add` snippet, the audit log, the daily egress budget (facts and bytes) and variant lookup cap, and the 8 tools with the scopes from [mcp.md](../design/mcp.md). |

![Health at a glance](screenshots/01-dashboard.png)

![Biomarker detail](screenshots/02-biomarker.png)

![Timeline](screenshots/03-timeline.png)

![Genome](screenshots/04-genome.png)

![Drug check](screenshots/05-drug-check.png)

![Wearables](screenshots/06-wearables.png)

![Imaging](screenshots/07-imaging.png)

![Import and review](screenshots/08-import-review.png)

![Family](screenshots/09-family.png)

![MCP and access](screenshots/10-mcp-access.png)

## Notes on the content

- Clinical numbers and genotypes are plausible but invented. Guideline quotes and citations (CPIC clopidogrel 2022, PREVENT, SCORE2) are there to show how a citation looks; check them against the source before reusing them anywhere else. The neurovascular CPIC row is a placeholder, not a quote.
- OwnHealth runs no genome engine. Genome findings come from [OwnGenome](https://github.com/GeiserX/OwnGenome) (https://github.com/GeiserX/OwnGenome), which handles WGS and consumer chips.
- The LDL reference change value (±16%) and its CV inputs are illustrative (unverified).
- The radiology image is a drawn SVG placeholder, not a scan.
