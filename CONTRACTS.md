# CONTRACTS.md

The behaviour OwnHealth keeps from one release to the next: the REST routes, the migrations, the MCP
tools and the UI rules. 0.2.0 moved the server from Python to Rust without changing any of it. The crate
map, the Rust names of the shared interfaces and the formats that must stay byte for byte (hashes,
canonical JSON, cookies, blob layout) are in [server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md). The
design behind each item lives in `docs/design/`; this file only fixes names, owners and shapes. Change
a shape here only in the same commit that changes the code, and say so in the PR.

Sections 1 to 4 and 6 (lanes, file ownership, dependency rules, service interfaces, demo hooks) are in
[server/RUST-CONTRACTS.md](server/RUST-CONTRACTS.md) sections 1 to 4 and 6. The numbering below stays
so older references keep pointing at the right section.

## 5. REST API

All routes are under `/api`, JSON, and resolve `{p}` as a profile slug, `me` or a uuid through
`core::access::require`. Errors are `{"detail": {"code": ..., "message": ..., ...}}`: 401 `unauthenticated`,
403 `forbidden` with the missing scope or action, 404 `profile_not_granted` (with `allowed` slugs),
`blob_missing` (the row is there, its stored file is not) or `unknown`, 409 for lifecycle and review conflicts, 415 `unsupported_upload`. A router module is
`server/crates/ownhealth-api/src/routes/<file>.rs` with a `router()` that `routes.rs` merges.

`lang`, on every route that takes it, is a registry language (`ownhealth_core::langs::LANGS`, today
`es` or `en`, exact and lower case). Any other value, `fr`, `EN` and `en-US` included, is 422
`literal_error` at `["query", "lang"]` (`["body", "lang"]` for a body field), never a silent other
language. An empty `lang=` counts as absent. An absent `lang` keeps the route's default: English on
the timeline and the couple view; the first registry language (Spanish) on observations, the trend
and wearables; the caller's own language on the others (status, dashboard, drug check, next draw and
its sheet, calculators, due, prevention answers, gaps and the gap letter, and the summary sheet,
export and share link bodies). A stored language outside the registry reads the first registry
language: only a `lang` the caller sent is refused. MCP tools take no REST `lang`; they speak the
token's `mcp_lang`.

| Owner | File (`routes/`) | Route | Notes |
|---|---|---|---|
| foundation | core.rs | `GET /me` | user, household, locale, dev flag; each profile has its `role`, `scopes` and `break_glass` (an emergency grant, which never shares) |
| foundation | core.rs | `GET /profiles` | `ProfileSummary[]` the caller can read, with role and scopes |
| foundation | core.rs | `GET /sources/{src_id}` | mcp.md 5.7 shape; `unknown` 404 when not visible |
| foundation | review.rs | `GET /review?status=&profile=&kind=&document_id=` | |
| foundation | review.rs | `GET /review/{id}` ; `POST /review/{id}/accept` `{edits?}` ; `/reject` `{reason}` ; `/reopen` ; `/dismiss` `{reason?}` | action `review` |
| foundation | review.rs | `POST /review/bulk-accept` `{document_id, min_row_conf}` | only items with no reasons |
| foundation | imports.rs | `POST /imports` multipart `profile`, `files[]`, `timezone?` | returns `{items: ImportItem[]}`; action `import`. `timezone` is the browser's IANA zone: a CGM export's job carries it and reads the file's times in it when the server has no `OWNHEALTH_TIMEZONE` or one Postgres does not know. A name Postgres knows travels as it is (`Europe/Oslo`), an old name Postgres lacks (`Asia/Calcutta`) travels as its current one (`Asia/Kolkata`), and a zone Postgres does not know travels as none; a zone never refuses an upload |
| foundation | imports.rs | `GET /jobs?ids=1,2` | the `result` of an `import.fit`, `import.gadgetbridge` or `import.cgm_csv` job (`WEARABLE_JOBS`) is `null` without `read:wearables` on the job's profile; the job itself stays visible |
| — | app.rs | `GET /healthz`, `GET /readyz` (outside `/api`) | |
| A | observations.rs | `GET /profiles/{p}/observations?q=&loinc=&alias_group=&category=&from=&to=&units=&lang=` | `{items: ObservationOut[]}` |
| A | observations.rs | `GET /profiles/{p}/trend?code=&window=P3Y&units=&lang=` | `Trend` (mcp.md 5.3 shape) |
| A | observations.rs | `POST /aliases` | `{profile, alias, alias_group, lang?, loinc?, unit?, specimen?}`, action `review`; re-derives the household |
| B | documents.rs | `GET /profiles/{p}/documents?q=&type=&limit=&latest=fhir` ; `GET /documents/{id}` | `latest=fhir`: only documents a FHIR import read, newest stored first |
| B | documents.rs | `GET /documents/{id}/file` ; `/pages/{n}.png` ; `/pages/{n}/words` | words need `read:documents_text` for tokens; a document whose flags hold a CGM import (`cgm_csv`) needs `read:wearables` whatever its `doc_type`, in the list too |
| B | documents.rs | `POST /documents/{id}/assign` `{profile}` ; `POST /documents/{id}/retract` `{reason}` | a moved CGM export (`flags.cgm_csv`) is imported again on its new profile by `import.cgm_csv`, in the zone its file keeps |
| C | records.rs | `GET`/`POST /profiles/{p}/records/{kind}` ; `POST .../{kind}/{id}/correct` ; `.../{id}/retract` | |
| C | records.rs | `POST /profiles/{p}/manual-observations` | |
| C | records.rs | `POST /profiles/{p}/records/medication/{id}/label` multipart `file` | a label photo (PNG, JPEG or WebP, at most 15 MiB) stored as a document (doc_type `other`, no job) and linked to the current version; write role; medication reads list `labels` |
| C | wearables.rs | `GET /profiles/{p}/wearables/summary` ; `GET /profiles/{p}/wearables/{metric}?window=` | an HRV series (and the summary HRV entry) is one source for the whole window; `limitations: string[]` is there only when another source was left out |
| D | genome.rs | `GET /profiles/{p}/genome` ; `GET /profiles/{p}/genome/variant?rsid=` ; `POST /profiles/{p}/genome/consent` | consent needs step-up |
| D | genome/events.rs | `GET /profiles/{p}/genome/events` | read:genome; the ClinVar diff's events, each named or withheld by the reader's gate; `release` (newest release compared, or null), `weekly` (the household's ClinVar check is on) and `rerun` `{en, es}` (OwnGenome's command for a reclassified call) |
| E | status.rs | `GET /profiles/{p}/status?as_of=` ; `GET /profiles/{p}/dashboard?lang=&as_of=` ; `GET /rules` | |
| E | drug_check.rs | `GET /profiles/{p}/drug-check?drug=&lang=` | |
| F | mcp_usage.rs | `GET /tokens/{id}/usage` | `/.well-known/oauth-protected-resource` lands in v0.2 |
| G | auth.rs | passkey register/login, OIDC, `POST /auth/logout`, `POST /auth/step-up` | |
| G | tokens.rs | `GET /tokens` ; `POST /tokens` (returns plaintext once) ; `DELETE /tokens/{id}` | |
| G | audit.rs | `GET /audit?profile=&limit=&before=` | |
| G | family.rs | `GET /household` (with `age_of_majority`, `transfer_pending` and `former_access`) ; `PUT /household/settings` `{age_of_majority}` (household owner, step-up, 14 to 21) ; `PUT /grants` ; `DELETE /grants?user_id=&profile=` ; `PUT /profiles/{p}/locks` ; `PUT /profiles/{p}/sensitive` ; `POST /household/profiles` ; `POST /household/invites` ; `DELETE /profiles/{p}?confirm=<slug>` ; `DELETE /account?confirm=<name>&delete_profile=` | |
| G | export.rs | `POST /profiles/{p}/export` `{fmt}` | step-up; answers the zip itself |
| G | auth.rs | `/auth/state`, `/auth/setup/{begin,finish,password}`, `/auth/login/{begin,finish}`, `/auth/recovery`, `/auth/password/login`, `/auth/step-up/{begin,finish}`, `/auth/passkeys` (GET, `/begin`, `/finish`, `DELETE /{id}`), `/auth/recovery-codes`, `/auth/invite/{code}`, `/auth/enroll/{begin,finish,password}`, `/auth/oidc/{login,callback}` | 403 `step_up_required`, 403 `csrf_failed` |
| H | timeline.rs | `GET /profiles/{p}/timeline?types=&from=&to=&as_of=&lang=` | |
| v1.0 sharing | shl.rs | `GET`/`POST /profiles/{p}/shl` `{categories?, days?, passcode?, label?, lang?}` ; `GET /profiles/{p}/shl/{id}/access` ; `DELETE /profiles/{p}/shl/{id}` | owner or guardian only, never break-glass (else 403 `not_sharer`); a category no link can carry (outside `SHAREABLE`, such as `read:genome` or `read:sensitive`) is 422 `literal_error`, a shareable category this creator cannot share on this profile (a lock, a narrower grant) is 403 `category_closed`, and a summary closed to them (a lock on it, or a grant without it; every link carries it) is 403 `summary_closed`; the list answers `{items, open}`, where `open` is what a new link of this caller may carry (their categories minus the profile's locks, the owner's own too, within `SHAREABLE`), so a form offers exactly what create accepts; create needs step-up and answers `{link, shlink, left_out?: {sensitive}}` once (the key exists only in the link; `left_out.sensitive` counts the sensitive records left out, and is absent unless the creator holds `read:sensitive`; the audit row never holds it); each listed link has `state` (`active`, `paused` while sharing is off, `revoked`, `expired`, `locked_out`) and `reason` (`sharing_off`; for revoked `person`, `locked` or `access_changed`); a lock on a category, or a grant narrowed below it, revokes every link that carries it for good (a grant that ran out ends its links even when it is extended before any sweep); revoke deletes the file and answers the same twice; list and access are audited; a 422 never repeats a passcode, password, pin, secret or token field, on any route and in any spelling (`passCode`, `pass_code`), nor a recovery or invite code (`recovery_code`, `invite_code`, and `code` on the recovery and enroll routes) |
| v1.0 sharing | shl.rs | `POST /shl/{id}` `{recipient, passcode?}` ; `GET /shl/{id}/file/{ticket}` | public, no cookie, CORS: the SMART Health Links manifest (401 `{remainingAttempts}`; 404 while sharing is off, once revoked, expired or out of attempts, or once the creator may no longer share every category it carries (a lock or a narrower grant revokes it at once, for good); 429 with Retry-After in seconds until the hour has room) and the file (`application/jose`, for at most 1 h); the only routes deploy/Caddyfile's public site block passes, each rewritten to its canonical path |
| J | app-level | `GET /metrics` (Prometheus, outside `/api`) | |
| C | imaging_proxy.rs | `POST /profiles/{p}/imaging/studies/{uid}/open` ; `GET /ohif/t/{capability}/...` and `GET /dicom-web/t/{capability}/...` (outside `/api`) | a browser session with `read:records` only (a token gets 403 `forbidden`); returns `{viewer_url, expires_in}`, a one-hour link bound to the session, the study and its one Orthanc study (a second open of the study in the same session returns the same live link, with the seconds it has left, and a new one in its last 10 minutes unless the server is full; one session holds at most 50 links); 403 `study_not_bound` unless Orthanc holds exactly that one study under the UID; one `imaging.open` audit row per study per session; design in docs/design/architecture.md "Imaging viewer and DICOMweb proxy" |
| C | imaging.rs | `POST /imaging/import` multipart `profile`, `files[]` (one zip, or a CD folder's files, at most 20,000) | Repeating `profile` or sending a file before it returns 400 `bad_request`. The app checks the `import` action on the profile before spooling files. 503 `imaging_off` without Orthanc; 413 `too_many_files` (a folder of more than 20,000 files, or a zip of more than 100,000 entries); 422 `invalid_zip`; returns `{import_id, duplicate, job_id, files}`: the upload is stored as one zip of the profile, and the `import.dicom` job sends its studies to Orthanc (follow it with `GET /jobs?ids=`; its file name and result need `read:records`). A study held for an identity, text encoding or UID issue waits as an `imaging_study` review item, shown only with `read:records` (`payload.reason` `identity_mismatch` or `second_patient`, which a person may accept, or `other_profile`, `uid_conflict`, `identity_conflict` and `unsupported_charset`, which a person may dismiss); the same zip reuses the import. For a recorded study, the job skips resending the disc when Orthanc holds every listed series with at least the listed instance count, and reconciles any unfinished record or report update. Otherwise the job sends the files and Orthanc deduplicates them. The job counts Orthanc submissions in `submitted_files`, rejected files in `refused_files`, and distinct retained instances in `instances` for newly imported studies. The clinical record uses the actual Orthanc series and instance counts. A study Orthanc refuses entirely is counted as refused with no review item. A StudyInstanceUID is on one record in the whole install: the same study offered to a profile of another household also waits as `other_profile`, which names no holder |

The TypeScript shapes for every route are in `ui/src/lib/api/types.ts`; the client functions are in
`ui/src/lib/api/{core,health,manage}.ts`. A server change that alters a response shape updates the type in
the same PR.

## 7. Migrations

Plain numbered SQL in `migrations/NNNN_name.sql` (embedded in the binary), applied in number order,
forward-only, each file once, its checksum recorded in `schema_migrations`. An edited applied file or
an applied version the code does not know stops startup. Each lane numbers inside its range so two lanes
never collide:

| Range | Owner |
|---|---|
| 0001-0099 | foundation (0001_foundation.sql: the whole v0.1 schema) |
| 0100-0149 | A |
| 0150-0199 | B |
| 0200-0299 | C |
| 0300-0399 | D |
| 0400-0499 | E |
| 0500-0599 | F |
| 0600-0699 | G |
| 0700-0799 | J |
| 1000-1079 | v0.2 Inboxes: foundation 1000-1009, then lanes A-G in steps of ten (server/RUST-CONTRACTS.md 10.3) |
| 1100-1109 | v0.3 Insight (0.4.0), records: medication fields, decisions and labels |
| 1110-1119 | v0.3 Insight (0.4.0), family: relatives, links and sharing consent |
| 1120-1129 | v0.3 Insight (0.4.0), genome: ClinVar snapshots and genome events |
| 1130-1139 | v0.3 Insight (0.4.0), prevention: decisions, answers and calendar feeds |
| 1200-1209 | v1.0 Connected, foundation: every v1.0 kind, purpose and channel, connector clients and state (1200) |
| 1210-1219 | v1.0 Connected, records: SMART, Blue Button, C-CDA, SHL and SHC receive, GEDCOM |
| 1220-1229 | v1.0 Connected, wearables: Open Wearables, Dexcom, vendor scores, sync gaps |
| 1230-1239 | v1.0 Connected, imaging |
| 1240-1249 | v1.0 Connected, keys: per-profile data keys and crypto-shredding |
| 1250-1259 | v1.0 Connected, audit: hash chain and session-report columns |
| 1260-1269 | v1.0 Connected, break-glass |
| 1270-1279 | v1.0 Connected, sharing: SMART Health Links and the ICE card |
| 1280-1289 | v1.0 Connected, ops: languages, partitions and other operations |
| 1290-1299 | v1.0 Connected, after 1280: per-profile and pending-item data keys (1290, 1291), CGM CSV exports as wearables documents (1292) |

H and I have no migrations. Every new phase numbers after the highest shipped file: the migrator applies
missing files in number order, so a lower number would run before files it depends on. Inside a phase,
a file depends only on files with lower numbers, and two files never share a number (the migrator
refuses to start). Prefer using the 0001 tables; add columns only when the design needs them.

Some CHECK constraints list allowed values: `source.kind`, `secret.purpose`, `auth_challenge.purpose`,
`document_source.channel`, `src.kind`, `review_item.kind` and `document.doc_type`. A migration re-creates such a CHECK with
the full union of every value shipped so far. A list with only one lane's values silently drops the
values of the other lanes. From 1200 on, each of these lists has a Rust constant, and
`server/crates/ownhealth/tests/check_mirror.rs` fails when a CHECK and its constant differ.
The migrator applies every missing file in number order. On a fresh database or a release upgrade
the highest-numbered re-creation runs last and wins. A dev database that already ran a higher file
applies a lower file that merged late last, so that file wins there. In v1.0 1200 changes the first
six lists, and 1221 adds `wearables` to `document.doc_type`. A later value goes into a new file numbered above every file that re-creates that CHECK,
and `check_mirror.rs` is the gate.

Lifecycle rule for every clinical table: only `status`, `status_reason` and `superseded_at` change after
insert (a trigger enforces it), and `audit_event` is append-only.

## 8. MCP tools

The server is `ownhealth_mcp::router(rt)` (rmcp), streamable HTTP at `/mcp`, stateless, JSON
responses; protocol 2026-07-28 with 2025-11-25 accepted. Full schemas and examples: `docs/design/mcp.md` section 5; the table below is the contract
summary every lane builds against.

| Tool | Scope | Input (beyond `profile`) | Output `data` | Budget | Backed by |
|---|---|---|---|---|---|
| `get_health_status` | `read:summary` (+ domain scopes widen sections) | `as_of` | `rules_version`, `state_hash`, `flags[]`, `medications[]`, `supplements[]`, `active_conditions[]`, `never_measured[]`, `stale[]`, `pending[]`, `corrections[]`, `notes[]`, `data_freshness{}` | < 2 KB | E `build_state` + `render` |
| `search_observations` | `read:labs` / `read:vitals` / `read:wearables` by category | `query` or `loinc`, `category`, `from`, `to`, `flagged_only`, `group_by`, `detail`, `cursor`, `as_of` | `results[]` `{name, code, value, unit, canonical, range, flag, date, lab, draw_id, src}`, `resolved` | 4k, max 15k | core `list_observations`, A |
| `get_trend` | as above | `code`, `window` (`P\d+[DMY]`/`all`), `resolution`, `as_of` | `name, codes, canonical_unit, resolution, coverage_pct, points[], stats, annotations[]` | 2k, max 6k | A, C rollups |
| `get_genome_findings` | `read:genome` (+ `read:sensitive` and the agents switch for sensitive) | `category`, `gene`, `min_evidence`, `detail`, `cursor`, `as_of` | `assay`, `findings[]` (each with `call_state`, and `clinvar_now` `{class, stars, release}` once a ClinVar release on or before `as_of` was compared, behind the same gate as the record's event) ; `not_assessed[]` ; `db_versions` | 4k, max 15k | D `list_findings` |
| `lookup_variant` | `read:genome_raw` | one of `rsid`, `hgvs`, `region`; `cursor` | `query`, `variant`, `calls[]` (`call_state` always), `annotations`, `cited_by_findings`; `quota` | ~500 per position | D `lookup_variant` |
| `check_drug` | any of `read:genome`, `read:records`, `read:labs` (missing parts in `withheld.by_scope`) | `drug` | `drug`, `pgx[]`, `phenoconversion[]`, `current_medications_relevant[]`, `relevant_labs[]`, `guidance[]`, `not_assessed[]` | 2k, max 4k | E `check_drug`, D `pgx_for_drug` |
| `get_source` | scope of the underlying fact; text needs `read:documents_text` | `src_id`, `include_text` | `src_id, status, document, location, printed, derived, extraction, crop_url, text` | 800, 4k with text | core `resolve_src`, B `page_words` |
| `propose` | `write:propose` + read on the profile | `kind` (`observations`, `note`, `finding_status`, `medication`), `payload`, `based_on[]` | `proposal_id, status: queued_for_review, kind, rows[]/..., review_url` | 1k | core `create_item` (kind `agent_proposal`), A, B `ground_value` |

Rules every tool follows (mcp.md 3, 9):

- Envelope fields: `profile, as_of, generated_at, lang, identity_mode, data, evidence, limitations,
  not_assessed, withheld{sensitive, by_scope, minor_rule, vendor_terms?}, db_versions, next_cursor,
  budget{tokens_est, truncated}, notice`, plus a compact markdown text block with the same facts.
  `withheld.vendor_terms` (`search_observations`, `get_trend`) counts the wearable days held by a
  provider that `data/connectors/providers.yaml` withholds. The field is absent when no days were
  withheld for vendor terms. An empty `search_observations` result means not measured in the requested
  window only if `withheld.vendor_terms` is absent.
- `tools/list` shows only tools whose scope the token holds. All tools: `openWorldHint: false`; all but
  `propose` are `readOnlyHint: true, idempotentHint: true`.
- Every fact carries `src`. Ids are stable and resolve through `get_source`.
- Free text is `{"trust": "untrusted_text", "origin": <pdf_text_layer|ocr|email_subject|fhir_note|
  agent_note|person_note>, "text": ...}` via `core::text::untrusted`.
- Tool errors are `isError: true` with `code`, `message`, `fix` (`profile_not_granted`, `scope_missing`,
  `rate_limited`, `egress_budget_exhausted`, `genome_lookup_cap`, `propose_cap`, `bad_input`).
- Limits per token (mcp.md 9.4): 60 calls/min burst 20; 2 000 facts and 2 MB per day; 200 lookups per
  day; 50 proposals per day, 200 rows per call. Counters live in Postgres.
- Every call writes one `audit_event` (`via = "mcp"`, `tool`, codes and ids only, free-text args
  hashed with `hash_arg`).
- `propose` never auto-accepts; failed rows are queued and marked, never dropped.

Resources (`cacheScope: private`, audited like tools): `ownhealth://profiles`,
`ownhealth://profile/{id}/status`, `ownhealth://profile/{id}/status.md`, `ownhealth://source/{src_id}`,
`ownhealth://rules/{version}`, `ownhealth://profile/{id}/access-log` (owner or guardian).
Prompts: `prepare-doctor-visit`, `check-before-new-drug`, `what-to-measure-next`.

## 9. UI

`docs/mockup/index.html` is the visual spec: the palette, type, cards, chips, sparklines, layout and
copy are ported into `ui/src/lib/styles/app.css` and the shared components. Pages use those classes and
components; a page that needs a new shared look adds it to a component, not a one-off style.

- Shared components (`#lib/components`): `Card`, `StatusChip` (status from the API, never computed),
  `PhaseChip`, `SourceChip` (every value shows its src id and opens `GET /sources/{id}`), `Sparkline`,
  `TrendChart` (hand-written SVG), `PageHeader`, `StubPage`.
- i18n: `t(key)` from `#lib/i18n/index.svelte`, es and en, every key in both (`_esComplete` check).
- State: `app.profile` (current slug) from `#lib/state/app.svelte`; pages read it and refetch.
- Imports use `#lib/...` (SvelteKit 3 subpath imports, `package.json` `imports`).
- `pnpm run check` (svelte-check, warnings fail) and `pnpm build` (into `server/static`)
  must pass.
