# Rivals: self-hosted PHRs, health MCP servers and commercial dashboards

Snapshot as of 2026-11-07; star counts drift.

Stars, licences, last-push dates and archive flags come from each project's GitHub page on 2026-11-07. Feature claims come from each project's README or source. Anything marked (unverified) was not checked against a primary source.

Contents:

1. Mirobody, the main rival
2. Self-hosted PHR projects
3. Clinical platforms (components, not competitors)
4. Wearables and device layers
5. Documents and imaging
6. Health MCP servers
7. Genome tools
8. Commercial dashboards
9. Features to adopt
10. Where everyone is weak
11. Open decisions

---

## 1. Mirobody (thetahealth/mirobody), the main rival

| Fact | Value |
|---|---|
| Stars / forks | 1,409 / 253 |
| Created / last push | 2025-11-29 / 2026-11-07 (very active, version 1.6.0) |
| Licence | Apache-2.0; terminology licences (LOINC, UCUM, ICPC-3, CPIC) listed in `LICENSE-3RD-PARTY` |
| Stack | Python 3.12, PostgreSQL 16 + pgvector, separate worker, web frontend, llama.cpp for local models, LangChain `deepagents` for the chat agent |
| Owner | Theta Health Inc. (Redwood City, CA). Mirobody is "the health data engine behind Theta", their hosted consumer app |

### What it does

Pipeline: Collect, then Standardize, then Agent.

- **Collect** (`mirobody/collect/`)
  - 23 file types: PDF, images (HEIC, TIFF), xlsx, docx, pptx, text, csv, json, xml, html.
  - Apple Health export import.
  - Garmin (OAuth1), Oura and Whoop (OAuth2), one drop-in folder per provider under `collect/providers/`.
  - Raw genotype files: 23andMe, AncestryDNA, WeGene, MyHeritage, FTDNA, single-sample VCF on GRCh37/38 (plain, gzip, BGZF, ZIP).
  - Free-text journal: "headache, BP 150/95, no fever" becomes one ICPC-3 complaint and two coded readings, with the negation respected.
- **Standardize** (`engine/`, `standardize/`)
  - Offline, no model. Bundled LOINC 2.83 cut of 63,416 codes with 1,023,273 alias names in 21+ languages, plus `resolver_overrides.tsv`.
  - 331 UCUM units with dimensional analysis. The unit picks the code: total cholesterol in mmol/L is 14647-2, in mg/dL is 2093-3.
  - Crosswalk of 902 device and lab indicators.
  - Published to PyPI (`pip install mirobody`, numpy is the only dependency) and runnable as a stdio MCP (`uvx mirobody mcp`).
- **Document reading**
  - PDF text layer page by page; only scanned pages go to a vision model.
  - Tables in born-digital PDFs rebuilt from character positions and read by rules.
  - Results cached by content hash.
  - Local OCR model GLM-OCR-0.9B; local answer models MiniCPM5-2B or Qwen3.8-27B (about 14.5 GB). Full local mode needs 16 GB RAM.
  - Each reading keeps the printed range and a high/low/normal flag (since 1.5.4), plus a citation to file and page.
- **Family ("care circles")**: each member chooses whether others may view or edit; a "managed" record for a parent who never signs in can later be claimed by that parent.
- **MCP**
  - Per-user remote MCP: Settings, "MCP link", valid 10 days by default.
  - Record tools: `query_health_indicators`, `query_medications`, `query_genetic_data`, `query_pharmacogenomics`.
  - Terminology tools: `resolve_indicator`, `normalize_unit`, `convert_unit`. Optional knowledge tools: `search_medical_knowledge`, `read_medical_source`.
  - stdio server: `standardize_reading`, `standardize_complaint`, `standardize_report`, `resolve_indicator`, `normalize_unit`, `convert_unit`.
  - Listed in the official MCP Registry as `ai.thetahealth/mirobody`; ships a Claude plugin and an agent skill.
- **Data model** (`mirobody/schema/`)
  - Core: `th_observation`, `th_files`, `th_extraction`, `th_concept`, `th_coding_alias/current/decision/history`, `th_data_source_priority`, `th_day_authority` (which device wins on a given day).
  - Genotype: `th_genotype_set`, `th_genotype`, `th_pgx_result` (reserved). Medication: `th_medication_plan`, `th_medication_course`, `th_dose_event`.
  - No first-class tables for conditions, allergies, immunizations, procedures, encounters or imaging.
- **Deployment**: Docker Compose (`COMPOSE_PROFILES=local-cpu`), native Debian install, or Codespaces. Public benchmarks on Hugging Face (ESL-Bench, MedHall-Bench, MedHarm-Bench).

### Weak spots (most from its own `docs/roadmap.md`)

1. **Thin genome by design.** Site index of 489 dbSNP sites in 12 pharmacogene regions; 262 of 625,705 rsIDs covered on one chip; no star-allele calls (every phenotype `not_determined`); no ClinVar, carrier screening, PRS, HLA, APOE/Lp(a) or WGS; multi-sample VCFs refused; CPIC extract pinned at v1.60.0 with no age shown in answers.
2. **No portal or FHIR import.** `health_data_epic` and `health_data_oracle` tables exist but nothing writes to them. The only FHIR output is a Genomics export of variant facts.
3. **No imaging.** No DICOM, no viewer.
4. **Silent data loss.** A unit that does not convert is stored as given with no log line. Readings rejected by the quality gate have no quarantine and no UI.
5. **Resolver gaps.** On a real Chinese hospital PDF, 51 of 110 indicators were coded; extraction renders names into English first, which loses codes the original spelling would resolve. On their demo report, 8 of 12 resolve.
6. **Self-disclosed security debt.** OAuth `state` is bound to the account, not the browser (RFC 6749 section 10.12), so one user's vendor tokens can land in another account. The MCP layer is a hand-written JSON-RPC dispatcher; the move to the SDK has not shipped.
7. **Embedded chat agent at the centre**: deepagents, a JS sandbox for cloud models, a Python sandbox for local ones. This brings a 22 KB model config and the 16 GB RAM requirement.
8. **Company funnel.** The open edition points to the hosted Theta app (live user-count badges, a Theta call to action). Wearable connectors need the operator's own developer apps. The roadmap follows China, Japan and US priorities (WeChat mini program, CN/JA vocabulary first).
9. **Missing sources**: Health Connect, Withings, Fitbit/Google Health, Polar, Dexcom/Libre (the `health_data_libre` table is empty), national portals, EU IPS.

### What we must match

- Offline LOINC/UCUM resolution with the unit picking the code.
- A citation from every value to its file and page.
- Text layer first, OCR second, cached by content hash.
- Rule-based table reading for born-digital PDFs.
- The printed reference range plus a flag.
- Family profiles with "managed until claimed".
- A per-person scoped MCP, listed in the MCP Registry.
- A one-command trial with synthetic demo data.
- Measurable public claims.

Only part of their resolver bundle is Apache-2.0. The hand-written overlays (`resolver_overrides.tsv`, `recall_synonyms.tsv`, `zh_curated.tsv`) are Apache-2.0; the LOINC 2.83 bundle and its language files fall under the LOINC License, and `ucum-essence.xml` under the UCUM License 1.1. Some files ship through Git LFS. We can reuse each part under its own licence, with attribution.

https://github.com/thetahealth/mirobody/blob/main/LICENSE-3RD-PARTY

https://github.com/thetahealth/mirobody
https://mirobody.ai/
https://www.crunchbase.com/organization/theta-health

---

## 2. Self-hosted PHR projects

| Project | Stars | Last push | Licence | What it is |
|---|---|---|---|---|
| fastenhealth/fasten-onprem | 2,793 | 2026-02-13, archived (archive date 2026-07-18 unverified) | GPL-3.0 | FHIR R4 family PHR viewer. Issue #629 (2026-02-07) dropped direct EHR pulls because the two stacks had diverged; Fasten Connect, the commercial product, keeps them. Now manual entry or FHIR bundle upload only |
| jwilleke/yourphr | 17 | 2026-11-05 | GPL-3.0 | Community continuation of Fasten. Single image `ghcr.io/jwilleke/yourphr:3.2.0`, optional DB encryption at first start, better non-US-Core FHIR display |
| karamhouse/chartrove | 0 | 2026-11-07 | GPL-3.0 | Second Fasten fork tracking upstream issues |
| LifeValue/HealthWallet.me | 73 | 2026-10-19 | GPL-3.0 | Flutter mobile PHR. On-device llama.cpp turns paper scans into FHIR resources; Fasten backend for US providers; IPS export; Apple/Google Wallet emergency card; offline proximity sharing (view-only on the receiver) |
| afairgiant/MediKeep | 554 | 2026-11-07 | NOASSERTION | React + FastAPI + Postgres. Medications, report builder. Manual entry first; no MCP in the README |
| cfu288/mere-medical | 323 | 2026-11-07 | MIT | Offline-first web PHR that syncs US patient portals itself over SMART on FHIR (Epic MyChart, Cerner, Athena, Veradigm, NextGen, Healow, VA). Single maintainer; pre-release since 2022 |
| DMJoh/Mediqux | 219 | 2026-11-01 | NOASSERTION | Family record: providers, appointments, prescriptions, lab PDFs, manual results against reusable reference panels, imaging files. One login scoped to several patients; RBAC |
| MBombeck/HealthLog | 282 | 2026-11-07 | NOASSERTION | Vitals PWA + iOS. Withings, WHOOP, Oura, Polar over OAuth2; Google Health; Nightscout CGM; Apple Health. WHO-5/PHQ-9/GAD-7 questionnaires, encrypted document vault, weather and pollen context, doctor PDF, read-only FHIR R4 API, revocable time-limited share links, reminders over APNs, Telegram, ntfy, Web Push |
| elkimek/get-based | 160 | 2026-11-07 | AGPL-3.0 | Browser local-first: labs, 51 curated SNPs, wearables, light/UV exposure, "Biology Scores", encrypted sync, password-protected share copy, optional "Agent Access" |
| potalora/strand | 7 | 2026-08-27 | MIT | Ingests FHIR, Epic EHI export, CDA/XDM and scans into one deduplicated FHIR timeline. De-identified context bundle, FHIR export, local MLX path |
| EnriqueNeyra/syntropy-health | 2 | 2026-11-07 | AGPL-3.0 | SMART on FHIR patient-access sync to Epic/Oracle, athenahealth, eClinicalWorks. C-CDA/CCD/XDM and Apple Health Records import, Oura/WHOOP via an OSS relay, local OCR, read-only logged `/mcp` endpoint |
| andronaft/health-os | 3 | 2026-11-05 | AGPL-3.0 | MCP-first local PHR on Postgres. Extraction done by the model in the client; labs staged as pending until the user approves. Deterministic critical-value alerts, Mann-Kendall trends, anomalies against the person's own baseline, screening calendar, SCORE2/ASCVD/FRAX stubs, safety rules sent on connect. Tool count 26 to 38 depending on version (unverified) |
| ashokkumarin/personal-health-record | small (unverified) | active (unverified) | AGPL | Family PHR: prescriptions, labs and bills on a timeline, with OCR |
| nickpdawson/OwnChart | 30 | 2026-06-22 | NOASSERTION | Life events plus records as an "AI research partner" |
| EduardPetraeus/HealthReporting | 5 | 2026-05-19 | NOASSERTION | DuckDB medallion (about 94 bronze sources, 49 silver tables). Typed MCP tools backed by YAML metric contracts; the agent never writes raw SQL |
| matthiasn/lotti | 1,205 | 2026-11-07 | GPL-3.0 | Private logbook with embedded agents (opposite philosophy, popular) |
| LHNCBC/phr | 38 | 2025-10-07 | NOASSERTION | NLM's old PHR, reference only |

What this group shows:

- Fasten, the largest star base, is archived. Its users are spread across forks with tens of stars.
- No project with traction combines records, labs, wearables, genome and MCP. Family PHRs exist (Mere Medical, Mediqux, health-os, ashokkumarin's PHR), but none leads and none has genome depth or non-US imports.
- Each project does one slice well: Syntropy and Mere for US portal sync, HealthWallet for IPS and the wallet card, HealthLog for devices and share links, health-os for approve-before-commit and deterministic safety, Strand for Epic EHI and CDA.
- health-os already uses the "outside agent extracts, human confirms" pattern. Our edge is genome depth, provenance and family use, not that pattern.
- Every records connector is US-only. None imports a Spanish, EU or LatAm national portal.

https://github.com/fastenhealth/fasten-onprem
https://github.com/fastenhealth/fasten-onprem/issues/629
https://github.com/jwilleke/yourphr
https://github.com/karamhouse/chartrove
https://github.com/LifeValue/HealthWallet.me
https://github.com/afairgiant/MediKeep
https://github.com/cfu288/mere-medical
https://github.com/DMJoh/Mediqux
https://github.com/MBombeck/HealthLog
https://github.com/elkimek/get-based
https://github.com/potalora/strand
https://github.com/EnriqueNeyra/syntropy-health
https://github.com/andronaft/health-os
https://github.com/ashokkumarin/personal-health-record
https://github.com/nickpdawson/OwnChart
https://github.com/EduardPetraeus/HealthReporting
https://github.com/matthiasn/lotti

---

## 3. Clinical platforms (components, not competitors)

| Project | Stars | Licence | Role for us |
|---|---|---|---|
| Medplum | 2,785 | Apache-2.0 | FHIR backend plus React kit. Too heavy for a family; `@medplum/fhirtypes` and FHIR React components worth borrowing. Its MCP repo `medplum/medplum-mcp-server` is archived |
| HAPI FHIR | 2,428 | Apache-2.0 | Java reference FHIR server. Not in core (no JVM); useful as a conformance oracle in CI |
| OpenEMR | 5,585 | GPL-3.0 | Clinic EHR; its portal assumes a clinic operator |
| OpenMRS core | 1,981 | MPL-2.0 (unverified) | Clinic EMR for low-resource settings; not person-centred |
| GNU Health 5.0 | n/a | GPL | Tryton-based, hospital-shaped. MyGNUHealth PHR 2.0 rewritten on Kivy; desktop/mobile app, not a web dashboard. Latest 5.0.7 on 2026-04-12 (unverified) |
| EHRbase | 388 | Apache-2.0 | openEHR server. openEHR is out of scope for us |

https://github.com/medplum/medplum
https://github.com/hapifhir/hapi-fhir
https://github.com/openemr/openemr
https://github.com/openmrs/openmrs-core
https://docs.gnuhealth.org/his/changelog.html
https://docs.gnuhealth.org/mygnuhealth/
https://github.com/ehrbase/ehrbase

---

## 4. Wearables and device layers

| Project | Stars | Licence | Notes |
|---|---|---|---|
| the-momentum/open-wearables | 2,767 | MIT | One schema and REST API over Garmin, Oura, Whoop, Polar, Suunto, Strava, Withings, Ultrahuman, SensorBio, Google Health and Fitbit (deprecated). No Coros, no Dexcom. Mobile SDKs for Apple Health, Health Connect, Samsung Health; large Apple XML import; outgoing webhooks. Built-in FastMCP server (`get_users`, `get_activity_summary`, `get_sleep_summary`, `get_workout_events`, `get_timeseries`, `get_menstrual_cycles`). Built for B2B apps (developer portal, API keys). We run it as the `wearables` sidecar instead of writing our own OAuth connectors, with its Strava provider disabled and any Strava sample dropped by our connector |
| soma-intelligence/open-health-sync | 62 | AGPL-3.0 | "Self-hosted Terra alternative" for Apple Health, Health Connect, Oura, WHOOP, Withings |
| nightscout/cgm-remote-monitor | 2,859 | AGPL-3.0 | De-facto CGM hub, REST API v1/v3. We read Nightscout, we do not replace it |
| arpanghosh8453/garmin-grafana | 3,540 | BSD-3 | Garmin to InfluxDB to Grafana. Shows demand for local Garmin history. Uses the unofficial `garminconnect` login, which breaks on auth changes (unverified for 2026) |
| dreeveapp/dreeve | 2,207 | AGPL-3.0 | Sports and fitness dashboard |
| Gadgetbridge | n/a | AGPL-3.0 | Android app; syncs Mi Band, Amazfit, Bangle.js, Pebble, some Garmin and Fossil watches with no vendor cloud; exports a SQLite DB. The only fully offline wearable path |
| Health Auto Export (HealthyApps) | n/a | commercial iOS app | Common route out of HealthKit: REST, MQTT and Home Assistant on Premium, documented JSON v2 payload. Has its own on-phone MCP server, but the app must stay in the foreground while a client is connected |
| owen282000/life-dashboard-companion-app | 94 | MIT | Health Connect to Home Assistant, MQTT or webhook, 34 data types, with backfill |

Smaller Apple Health bridges: PulsHealth (Apache-2.0, iOS app + sync protocol + read-only MCP), roian6/apple-health-ai-bridge, leblancfg/healthkit-relay (includes ECG and routes), BRO3886/healthsync (export to SQLite, 74 stars). Health Auto Export receivers: `HealthyApps/health-auto-export-server`, `irvinlim/apple-health-ingester`, `benniblau/health-mcp`, `Airbloomer/health-mcp`.

The pattern converges: an iOS companion app pushes to your server. A web app alone cannot read HealthKit.

https://github.com/the-momentum/open-wearables
https://github.com/soma-intelligence/open-health-sync
https://github.com/nightscout/cgm-remote-monitor
https://github.com/arpanghosh8453/garmin-grafana
https://github.com/dreeveapp/dreeve
https://gadgetbridge.org/
https://help.healthyapps.dev/en/health-auto-export/automations/server-connection/
https://github.com/Lybron/health-auto-export/wiki/API-Export---JSON-Format
https://github.com/owen282000/life-dashboard-companion-app

---

## 5. Documents and imaging

| Project | Stars | Licence | Notes |
|---|---|---|---|
| paperless-ngx/paperless-ngx | 47,835 | GPL-3.0 | Self-hosted document standard: OCR (Tesseract/ocrmypdf), consumption folder, mail fetch, tags, REST API, webhooks. Many target users already keep lab PDFs there, so a Paperless source (by tag or webhook) is cheap and high-value |
| Orthanc | n/a | GPL/AGPL | DICOM server with DICOMweb |
| OHIF/Viewers | 4,402 | MIT | Web DICOM viewer. Orthanc + OHIF is the standard self-hosted imaging pair; no PHR above ships it |
| gyorgysh/openradview | 24 | GPL-3.0 | Desktop viewer for your own DICOM/NIfTI with optional MCP agent access. The only "personal imaging + MCP" project found |

Among commercial products, only Guava accepts DICOM upload.

https://github.com/paperless-ngx/paperless-ngx
https://github.com/jodogne/OrthancDocker
https://github.com/OHIF/Viewers
https://github.com/gyorgysh/openradview

---

## 6. Health MCP servers

| Server | Stars | Licence | Scope |
|---|---|---|---|
| neiltron/apple-health-mcp | 575 | MIT | Local Apple Health export; sleep, workouts, heart queries |
| mvilanova/intervals-mcp-server | 388 | GPL-3.0 | Intervals.icu training |
| the-momentum/apple-health-mcp-server | 285 | MIT | Apple Health export via DuckDB |
| Nicolasvegam/garmin-connect-mcp | 190 | none | 61 Garmin tools |
| shashankswe2020-ux/whoop-mcp | 179 | MIT | Read-only WHOOP |
| healthymind-tech/Taiwan-Health-MCP | 168 | MIT | 63 tools; FHIR R4/TW Core validation, ICD-10, SNOMED, LOINC, drug DB. A national-profile MCP is a real pattern |
| thebriangao/totem | 139 | NOASSERTION | WHOOP read and write via the reverse-engineered private API |
| wso2/fhir-mcp-server | 137 | Apache-2.0 | Any FHIR server as MCP (search/read, SMART auth) |
| YuzeHao2023/MCP-oura | 116 | MIT | Oura API |
| the-momentum/fhir-mcp-server | 102 | MIT | FHIR MCP |
| jmandel/health-skillz | 82 | MIT per README; no LICENSE file in the repo | A Claude skill that connects to SMART on FHIR portals, by a SMART co-author |
| langcare/langcare-mcp-fhir | 71 | MIT | Enterprise FHIR EMRs |
| HealthyApps/health-auto-export-mcp-server | 68 | MIT | Node bridge to the Health Auto Export app |
| aks129/HealthClawGuardrails | 33 | MIT | PHI redaction, immutable audit and step-up auth between agents and FHIR |
| Eldergenix/GenomeMCP | 18 | MIT | ClinVar and gene reference queries, not the person's data |
| PhilipAD/health-export-mcp | 11 | MIT | 194 metrics, 22 tools, 23 MCP prompts |
| daveremy/function-health-mcp | 10 | MIT | Scrapes Function Health results into MCP. Shows people want commercial lab panels in an agent |
| natecostello/genechat-mcp | 7 | MIT | WGS VCF: `query_variant(s)`, `query_gene(s)`, `query_pgx`, `query_clinvar`, `query_gwas`, `calculate_prs`, `genome_summary`, `list_genomes`. Annotates once, then pysam lookups; states that tool output reaches the model vendor |
| peter-fusek/oncofiles | 5 | MIT | Medical records from Gmail and Drive to MCP, for cancer patients |
| alfonsoguv/deepdna-mcp | 0 | MIT | Cited CPIC/DPWG drug-gene lookups |
| csjohnson23/pgs-mcp | 0 | MIT | pgsc_calc match QC |

Directories: `davidmosiah/delx-wellness` lists local-first wellness MCP connectors. `VelkyVenik/missingmcp` hosts OAuth 2.1 MCP gateways for Garmin and WHOOP.

What the landscape shows:

- Almost every server covers one source: Apple Health, one wearable, one VCF or one FHIR endpoint.
- Cross-source servers (Mirobody, health-os, HealthReporting, Syntropy, Open Wearables) cover the genome at most as PGx-lite or a few SNPs.
- None returns provenance on every value, uses standard OAuth 2.1 MCP auth, and logs what the agent read. HealthClawGuardrails is the only one working on audit.
- No server offers one person-scoped MCP over records, labs, wearables, a full genome and imaging.

https://github.com/neiltron/apple-health-mcp
https://github.com/the-momentum/apple-health-mcp-server
https://github.com/wso2/fhir-mcp-server
https://github.com/aks129/HealthClawGuardrails
https://github.com/healthymind-tech/Taiwan-Health-MCP
https://github.com/jmandel/health-skillz
https://github.com/natecostello/genechat-mcp
https://github.com/daveremy/function-health-mcp
https://github.com/PhilipAD/health-export-mcp

---

## 7. Genome tools

| Project | Stars | Licence | Notes |
|---|---|---|---|
| PharmGKB/PharmCAT | 200 | MPL-2.0 | CPIC/DPWG star-allele caller |
| PGScatalog/pgsc_calc | 179 | Apache-2.0 | Nextflow PRS calculator; last push 2026-06-03 |
| codebase/gnomancer | 2 | MIT | Raw chip file to QC, haplogroups, traits, ClinVar, PharmCAT, one HTML report |
| ImYourBoyRoy/genomics-caddy | n/a | n/a | Desktop explorer for AncestryDNA and 23andMe with an MCP server |
| GeiserX/OwnGenome | 22 | GPL-3.0 | WGS to DeepVariant, SVs, ClinVar, CPSR, PharmCAT + pypgx, PRS, HLA, traits, family and couple carrier screening |

No rival PHR comes close to OwnGenome's depth. OwnHealth ingests its `genome-facts.json` and tabix-indexed VCF; it never runs genome engines.

https://github.com/PharmGKB/PharmCAT
https://github.com/PGScatalog/pgsc_calc
https://github.com/GeiserX/OwnGenome

---

## 8. Commercial dashboards

| Product | What it offers | Weak spot for our audience |
|---|---|---|
| Guava Health | Portal sync for 50K-100K US systems (MyChart, Cerner, Elation, Healow, VA); upload of PDF, image, DICOM, CCDA; normalized labs; family profile managers; AI summaries; 14 languages. Premium $8/month or $78/year | Hosted, US-centred. A reviewer reports that when two readings conflict on the same day, the latest-processed one wins |
| Heads Up Health | Pivoted to clinics. Labcorp, Quest, Rupa, InBody, Dexcom, Oura, Garmin, Withings; records from 30,000+ organizations. Custom "ideal panel" of chosen tests | Clinic product, hosted |
| Function Health | Membership blood panels, about $365/year for two draws, 114 biomarkers by an independent count | No API or MCP (hence the scraping MCP above) |
| Superpower | $349/year since 2026-09-01; Basic 59 biomarkers; upload of older PDFs | No API or MCP. Function sued Superpower for false advertising |
| Google Health app (May 2026) | Combines Fitbit, Pixel Watch, Health Connect, Apple Health and medical records, with a Gemini coach | Hosted, embedded AI, vendor-bound |
| InsideTracker, Levels, Bearable, Exist.io, Gyroscope, Humanity (unverified) | Blood + DNA "inner age" scores; CGM metabolic scoring; mood/symptom correlations with tracked data; life dashboards; biological-age estimates | Hosted; scores without published formulas |

Ideas worth taking from this group: a custom "ideal panel", one "how am I doing" status per domain, and auto-correlation across sources. We do not take the biological-age view (PhenoAge): the roadmap's Never list rules out a composite health score.

https://guavahealth.com/article/lab-integration-dashboard-for-providers
https://www.health3.app/compare/health3-vs-guava/
https://headsuphealth.com/functional-medicine-platform/
https://www.bloodtestcomparison.com/function-vs-superpower
https://sahha.ai/blog/google-health-launch-2026/

---

## 9. Features to adopt

Ranked by value divided by effort. The phase column matches our roadmap (ROADMAP.md).

| # | Feature | Seen in | Phase |
|---|---|---|---|
| 1 | Offline LOINC + UCUM resolver where the unit picks the code; resolve on the source spelling plus our es/en/de/fr alias table | Mirobody (Apache-2.0 overlays, LOINC and UCUM content under their own licences) | v0.1 |
| 2 | Provenance on every value: document, page, bounding box, shown in the UI and returned over MCP as `src` | Mirobody | v0.1 |
| 3 | Text layer first, OCR only for scanned pages, rule-read tables, content-hash cache | Mirobody | v0.1 (OCR profile v0.2) |
| 4 | Approve before commit: agent proposals and any row that fails the auto-accept rule land in a visible review queue; rejects are never dropped | health-os | v0.1 |
| 5 | Typed MCP tools backed by declared contracts, never raw SQL; safety rules in server instructions on connect; MCP prompts for common questions | HealthReporting, health-os, PhilipAD | v0.1 |
| 6 | Family profiles with "managed until claimed", per-member grants (our roles: owner, guardian, caregiver, viewer), one login over several profiles | Mirobody, Mediqux | v0.1 |
| 7 | FHIR R4/IPS bundle import; IPS export; emergency card | HealthWallet, HealthLog | v0.1 import, v1.0 ICE card |
| 8 | Device-day authority: which source wins steps or sleep on a given day | Mirobody `th_day_authority`, Syntropy | v0.1 |
| 9 | Paperless-ngx source and a mail-in address with sender allowlist | Paperless, Oncofiles | v0.2 |
| 10 | Nightscout for CGM; Health Auto Export JSON v2; Gadgetbridge SQLite | HealthLog, community bridges | v0.2 |
| 11 | Deterministic analytics: Mann-Kendall trend with slope per year, anomalies against the person's own baseline, critical-value alerts, screening calendar; range precedence condition, then sex, then default | health-os, Mirobody roadmap | v0.1 trend, v0.3 screening and baseline anomalies; critical values cut |
| 12 | Genome as findings: PGx diplotypes, ClinVar, carrier status, PRS with percentile and ancestry caveat, HLA, APOE, Lp(a); raw lookups through `lookup_variant`; `check_drug` joins PGx with current medications and labs | GeneChat, OwnGenome | v0.1 |
| 13 | SMART on FHIR patient-access sync; C-CDA/CCD/XDM and Epic EHI import | Syntropy, Strand, Mere | v1.0 |
| 14 | Wearable OAuth through Open Wearables instead of our own connectors | Open Wearables | v1.0 |
| 15 | Imaging: Orthanc + OHIF, studies linked to the radiology report | none in PHRs | v1.0 |
| 16 | Share links: time-limited and revocable, created from the UI only (never over MCP) | HealthLog | v1.0 (SMART Health Links; ciphertext on a host the user controls, we run no share host) |
| 17 | Correlations and context: weather, pollen, daylight; symptom-to-data correlation; PHQ-9 and GAD-7 questionnaires | HealthLog, Bearable, Exist | v2 |
| 18 | One-command trial with a synthetic demo family and measured public claims | Mirobody, Strand (Synthea) | v0.1 |

What we do not adopt: an embedded chat agent (Mirobody, Lotti, HealthLog, get-based, Guava), share or export tools over MCP, and a Strava API connector (Strava's 2026 API policy bans feeding its data into an AI context window, including via MCP). We import Strava bulk exports only.

https://www.strava.com/legal/api_policy

---

## 10. Where everyone is weak

1. **Records outside the US.** Every portal connector anyone ships targets US EHR vendors (Epic, Oracle Health, athena, eClinicalWorks, NextGen, Veradigm, Healow, VA); none targets a non-US portal. Nobody imports Spanish, EU or LatAm national portals, IPS sources or EHDS formats. In Spain, HCDSNS and the regional health apps hand out PDFs behind Cl@ve, so PDF import is the first step.
2. **Genome depth.** At best PGx-lite (Mirobody: 12 genes, phenotype `not_determined`) or 51 curated SNPs (get-based). Nobody joins WGS findings, carrier screening, PRS, HLA and family pedigree with medications and labs. OwnGenome (https://github.com/GeiserX/OwnGenome) produces those findings; OwnHealth joins them with the rest of the record.
3. **Imaging.** No self-hosted PHR ships a DICOM viewer.
4. **Silent data quality.** Unconvertible units stored as given, rejected rows invisible, no review step. Only health-os stages results for approval.
5. **MCP done as an afterthought.** Home-made remote auth (Mirobody's 10-day link and hand-written JSON-RPC) while the OAuth 2.1 MCP spec exists; results without provenance; no audit of what the agent read.
6. **Prompt injection.** No rival treats imported document text as untrusted when it returns it to an agent that also holds genome data and has outbound tools.
7. **Embedded chat agents** add RAM, model config and safety surface to most rivals. Data + UI + MCP only is simpler to run and to audit.
8. **Family history.** Relatives' conditions with age at onset and a pedigree view are absent everywhere.
9. **Medication and supplement history** with the reason for starting or stopping and a re-check date.
10. **Fragmentation and abandonment.** Fasten is archived and its forks are tiny; Mere is single-maintainer; Mirobody serves one company's hosted app.
11. **Multilingual lab reports beyond CN/JA/EN.** Spanish, Portuguese and other European lab spellings are thin; Mirobody's resolver grows one reviewed row at a time.

---

## 11. Open decisions

- **Mirobody resolver upstream.** We start from their resolver bundle: Apache-2.0 overlays (`resolver_overrides.tsv`, `recall_synonyms.tsv`) plus LOINC 2.83 content under the LOINC License and UCUM under the UCUM License. Open: do we track upstream releases or fork once? Tracking inherits their CN/JA-first alias priorities.
- **Orthanc licence fit.** Orthanc is GPL/AGPL and runs as a separate container in the `imaging` profile. Check that shipping it in our compose file fits AGPL-3.0-or-later.
- **Open Wearables footprint.** It is built for B2B (developer portal, API keys, its own storage). Measure what the `wearables` sidecar costs a single household before v1.0, and how many vendor OAuth apps a self-hoster must still register.
- **Oura and Garmin AI terms.** Oura's 2026 API and MCP agreement reportedly restricts AI use (unverified); Garmin reportedly allows it with disclosure and consent (unverified). Read both primaries before the v1.0 connectors.
- **Findings schema for other pipelines.** `ownhealth.genome-facts/v1` is OwnGenome's contract. Open: document it well enough that gnomancer or GeneChat annotations could emit it too.
- **Commercial claims.** Claims for InsideTracker, Levels, Bearable, Exist.io, Gyroscope and Humanity are unverified; check them before quoting them elsewhere.
