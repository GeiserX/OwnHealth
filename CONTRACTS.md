# CONTRACTS.md

The working agreement between the v0.1 lanes. The foundation (`feat: foundation`) ships the shared core,
and every interface another lane calls exists as a typed stub that raises `NotImplementedError` until its
owner lands it. Change a signature here only in the same commit that changes the code, and say so in the
PR. The design behind each item lives in `docs/design/`; this file only fixes names, owners and shapes.

## 1. Lanes

| Lane | Area | Design |
|---|---|---|
| A | normalize: value parsing, LOINC resolution, UCUM, conversions, ranges, flags | ingestion.md 8, architecture.md 10 |
| B | lab-pdf: text layer, tables, vendor templates, page renders, grounding | ingestion.md 5-7, 9 |
| C | apple + fhir + manual: Apple Health zip, FHIR/IPS bundles, manual entry, records and wearables APIs | connectors.md 2.2, 4; architecture.md 9 |
| D | genome: genome-facts import, findings, tabix lookups, gating | genomics.md |
| E | rules + status + check_drug: rule pack, `state_hash`, status words, dashboard, drug check | architecture.md 11, mcp.md 4, 5.6 |
| F | mcp: the 8 tools, PAT auth on /mcp, envelope, budgets, resources, prompts | mcp.md |
| G | security + auth + export: passkeys, OIDC, sessions, step-up, tokens UI API, audit API, family API, export | security-privacy.md, mcp.md 2 |
| H | ui-pages-1: dashboard, biomarkers, timeline, genome, drug check | docs/mockup |
| I | ui-pages-2: wearables, imaging, import and review, family, MCP and access | docs/mockup |
| J | ops + docs: backup, restore, metrics, README, compose profiles, release | architecture.md 13-15 |

## 2. Directory map and file ownership

A lane edits only the files it owns. Anything marked **foundation** is shared: change it in a small,
separate commit that touches nothing else, and keep existing signatures working.

```text
backend/
  pyproject.toml, uv.lock                  foundation (any lane may add a dependency; one line, sorted)
  src/ownhealth/
    __init__.py, __main__.py, cli.py       foundation (J may add subcommands: backup, restore, verify-restore)
    app.py, config.py, runtime.py          foundation
    blobs.py                               foundation
    db/__init__.py, db/migrate.py          foundation
    db/migrations/0001_foundation.sql      foundation, frozen: never edit, add a new numbered file
    db/migrations/NNNN_*.sql               the lane whose range holds NNNN (section 7)
    core/                                  foundation (access, acceptance, audit, documents, fhir, ids,
                                           lifecycle, observations, provenance, records, review)
    jobs/__init__.py                       foundation
    jobs/kinds/pdf.py                      B   (import.pdf)
    jobs/kinds/fhir.py                     C   (import.fhir_bundle)
    jobs/kinds/apple_health.py             C   (import.apple_health)
    jobs/kinds/genome.py                   D   (import.genome_facts)
    jobs/kinds/state.py                    E   (state.rebuild)
    jobs/kinds/normalize.py                A   (normalize.rederive)
    normalize/                             A
    ingest/__init__.py                     foundation (ImportSummary)
    ingest/pdf.py (or ingest/pdf/)         B
    ingest/fhir.py, apple_health.py, manual.py   C
    genome/                                D
    state/, rules/                         E   (rules/pack/*.yaml and rules/cpic/snapshot.json ship in the package)
    mcp/__init__.py                        F   (keeps build_server(settings) -> MCPServer)
    mcp/envelope.py                        F   (untrusted() is final; do not change its shape)
    mcp/tools/, mcp/resources.py, mcp/prompts.py   F
    auth/tokens.py, auth/principal.py      foundation (G may extend; keep signatures)
    auth/sessions.py, passkeys.py, oidc.py G
    export/                                G
    ops/                                   J
    api/_deps.py, core.py, review.py, imports.py   foundation
    api/<file>.py                          the lane in section 5
    demo/__init__.py, base.py, family.py, pdf.py   foundation
    demo/hooks/<lane>.py                   that lane (section 6)
    static/                                build output of ui/, never edited by hand, not committed
  tests/
    conftest.py, factories.py              foundation (lanes add fixtures in their own conftest)
    test_*.py (present at foundation)      foundation
    <lane>/                                that lane: tests/normalize, tests/pdf, tests/imports, tests/genome,
                                           tests/state, tests/mcp, tests/security, tests/ops
backend/src/ownhealth/normalize/terminology/   A (LOINC subset, aliases, molar masses; inside the package so it ships)
backend/src/ownhealth/ingest/pdf/vendors/<country>/<provider>.yaml   B (vendor templates)
fixtures/                                  every lane adds under fixtures/<lane>/; synthetic only
ui/
  package.json, vite.config.ts, tsconfig.json   foundation
  src/app.html, app.d.ts                   foundation
  src/lib/styles/app.css                   foundation (lanes append page-only rules in their component's <style>)
  src/lib/i18n/dict.ts                     shared: add keys in your own block, never rename others' keys
  src/lib/components/                      foundation (H and I may add new components; edit existing ones
                                           only additively, new optional props)
  src/lib/api/core.ts, types.ts            foundation (types.ts: add new types; change an existing one only
                                           with its backend owner)
  src/lib/api/health.ts                    H
  src/lib/api/manage.ts                    I
  src/lib/nav.ts, src/routes/+layout.*     foundation
  src/routes/+page.svelte, biomarkers/, timeline/, wearables/, imaging/, family/, lib/components/pages1/   H
  src/routes/genome/, drug-check/, import-review/, mcp-access/, documents/, settings/, login/,
    lib/components/pages2/                 I
Dockerfile, compose.yaml, .env.example     foundation, then J
.github/workflows/                         foundation, then J
README.md, docs/                           J (design docs: the lane that changes the design)
scripts/test-db.sh                         foundation
```

## 3. Dependency rules

- `core`, `db`, `blobs`, `jobs`, `config` import nothing from any lane package.
- Lanes import from `core` freely, and from another lane only the functions listed in section 4. Never
  import a lane's private module (`_name.py`) or a function that is not in this file.
- Every fact enters the store through `core.review.route_fact` / `submit_candidate` (imports) or
  `core.observations.commit_observation` / `core.records.add_record` with `accepted_by="person"`
  (manual entry, review accept). No lane writes `observation` or the record tables with raw SQL.
- Every fact has provenance: `core.provenance.SrcIn` (document region, import ref, manual, agent).
- Every read a person or agent makes on someone's data writes one `core.audit.write_audit` row in the
  same transaction.
- Every permission check goes through `core.access.require` / `effective_permission`. No lane
  re-implements `token scopes ∩ grant categories − locks`.
- Status words (OK / Watch / Out / Stale / No data) come only from lane E (`state.status_word` and the
  dashboard API). The UI never derives them.
- Free text going out over MCP is wrapped with `mcp.envelope.untrusted(text, origin)`.
- The UI talks to the backend only through `#lib/api/*`. Pages never call `fetch` themselves.
- No MCP tool sends data out of OwnHealth (mcp.md 9.2).

## 4. Service interfaces

Foundation functions are implemented and tested. Lane functions are stubs with these signatures.
`conn` is always a SQLAlchemy `Connection` inside a transaction; the caller owns the transaction.

### 4.1 Foundation (implemented)

```python
# config.py
Settings.from_env() -> Settings                      # OWNHEALTH_* and *_FILE twins
# runtime.py
runtime.get() -> Runtime(settings, engine, blobs)
# blobs.py
BlobStore(root).put_bytes(data) -> str               # sha256 hex; path blobs/sha256/ab/cd/<hash>
BlobStore.put_stream(stream) -> (str, int); .put_file(path) -> (str, int)
BlobStore.open(sha) / .get_bytes(sha) / .iter_chunks(sha) / .exists(sha) / .path(sha)
# db
migrate.migrate(engine, directory=MIGRATIONS_DIR) -> list[int]       # forward-only, checksum-locked
# core.access
require(conn, principal, ref, scope=None, action=None) -> Permission   # ref: slug | "me" | uuid | None
effective_permission(conn, principal, profile_id) -> Permission         # .scopes, .role, .has(s), .can(a)
accessible_profiles(conn, principal) -> list[Profile]
effective_scopes(token_scopes, categories, locks, role) -> frozenset[str]
create_household / create_user / create_profile(conn, ProfileIn) / set_grant / revoke_grant / set_locks
AccessDenied(code, message, allowed)                 # -> HTTP 404 profile_not_granted, else 403
# core.provenance
SrcIn(kind, profile_id, document_id?, page_no?, bbox?, import_ref?, label?, ...)
ensure_src(conn, SrcIn | str) -> str                 # src id ^src_[a-z0-9_]+$
resolve_src(conn, src_id) -> {src, profile_id, facts, document} | None
# core.observations
commit_observation(conn, ObservationIn, *, src, accepted_by, status="current", reviewed_by=None,
                   supersedes=None) -> Committed(id, src_id)
supersede_observation(conn, old_id, new, *, src, accepted_by, reason, reviewed_by=None) -> Committed
retract_observation(conn, id, reason) ; confirm_observation(conn, id)
list_observations(conn, profile_id, *, alias_group, loinc, category, since, until, as_of,
                  include_sensitive, limit) -> list[dict]
latest_per_group(conn, profile_id, *, as_of=None, category=None) -> list[dict]
observation_history(conn, id) -> list[dict]
# core.records  (kind: condition | medication | allergy | immunization | procedure)
add_record(conn, record, *, src, accepted_by, status="current", supersedes=None) -> Committed
supersede_record / retract_record(conn, kind, id, reason) / get_record / record_history / list_records
active_medications(conn, profile_id, *, on=None, as_of=None, kind=None, include_sensitive=False)
# core.acceptance
auto_accept(row_conf, checks, source, identity, threshold=0.95) -> Decision(auto, reasons, why_not)
Check(name, ok, reason=None, detail=None)            # reason in REASON_CODES
# core.review
route_fact(conn, *, observation, src, checks, row_conf, allowance, identity, threshold=None,
           provisional=False, candidate_row_id=None, extra_payload=None) -> RouteResult
create_extraction_run(conn, document_id, *, extractor, extractor_version, config_hash) -> UUID
submit_candidate(conn, run_id, CandidateIn, *, allowance, identity, threshold=None,
                 provisional=False) -> RouteResult(state, observation_id?, review_item_id?, reasons)
create_item(conn, ReviewItemIn) -> UUID              # agent proposals use kind "agent_proposal"
register_accept_handler(kind)(fn)                    # fn(conn, item, user_id, edits) -> list[UUID]
accept / reject(reason) / reopen / dismiss / bulk_accept(conn, *, document_id, user_id, min_row_conf)
# core.documents
create_document(conn, *, household_id, profile_id, blob_sha256, mime, size_bytes, filename, ...) -> DocumentRef
dispatch_upload(conn, blobs, *, household_id, profile_id, files, actor_user) -> list[dict]
sniff(filename, head, blob_path=None) -> pdf | image | apple_health_zip | fhir_json | fhir_ndjson |
                                         genome_facts | vcf | tbi | unknown
# core.audit
write_audit(conn, AuditEvent) -> int ; hash_arg(value) -> "h:<12 hex>" ; list_audit(conn, ...)
# jobs
@handler("import.pdf") def fn(ctx: JobContext, job: Job) -> dict | None
enqueue(conn, kind, payload, *, queue="core", dedupe_key=None, priority=5, run_after=None, max_attempts=5) -> int | None
PermanentJobError                                    # straight to dead + a job_failed review item
# auth
tokens.create_token(conn, *, user_id, label, profile_ids, scopes, default_profile=None, days=90,
                    identity_mode="pseudonymous", lang=None, units="as_reported") -> NewToken(id, plaintext, prefix)
tokens.verify_token(conn, plaintext) -> Principal | None ; revoke_token ; list_tokens
principal.resolve(conn, settings, *, authorization, session_secret) -> Principal | None
# mcp.envelope (final)
untrusted(text, origin) -> {"trust": "untrusted_text", "origin": origin, "text": text}
ToolError(code, message, fix).as_dict()
```

### 4.2 Lane stubs

| Module | Function | Owner | Called by |
|---|---|---|---|
| `normalize` | `parse_value(printed, *, locale, unit=None) -> ParsedValue` | A | B, C, F |
| | `resolve_analyte(conn, printed_name, *, lang, unit, specimen=None, method=None, provider=None) -> Resolution` | A | B, C, F |
| | `to_ucum(printed_unit) -> str \| None` ; `canonical_unit(alias_group)` | A | B, C, E |
| | `convert(value, from_ucum, to_ucum, *, alias_group) -> Decimal \| None` | A | E, API |
| | `parse_range(printed, *, locale) -> list[RefRange]` ; `pick_range(ranges, *, sex, age_years)` | A | B, C |
| | `interpret(value, text_code, ref) -> "L"\|"N"\|"H"\|"A"\|None` | A | B, C |
| | `normalize_row(conn, *, profile_id, printed_name, printed_value, printed_unit, printed_range, printed_flag, effective_at, lang, specimen=None, provider=None) -> Normalized(observation, checks)` | A | B, C, F |
| | `rederive(conn, *, profile_id=None, since=None) -> int` ; `DERIVATION_VERSION` | A | J |
| `ingest.pdf` | `import_pdf(conn, blobs, *, document_id) -> ImportSummary` | B | job `import.pdf` |
| | `page_words(conn, blobs, *, document_id, page_no) -> list[Word]` | B | F, API |
| | `render_page_png(blobs, *, blob_sha256, page_no, scale=2.0) -> bytes` | B | API |
| | `ground_value(conn, blobs, *, document_id, page_no, name, value) -> bbox \| None` | B | F (`propose`) |
| `ingest.fhir` | `import_fhir(conn, blobs, *, document_id) -> ImportSummary` | C | job `import.fhir_bundle`, Apple Health |
| `ingest.apple_health` | `import_apple_health(conn, blobs, *, document_id) -> ImportSummary` | C | job `import.apple_health` |
| | `rebuild_rollups(conn, *, profile_id, metric=None) -> int` | C | J |
| `ingest.manual` | `add_manual_observation(conn, *, profile_id, user_id, name, value, unit, effective_at, note=None) -> Committed` | C | API |
| `genome` | `validate_facts(doc) -> list[str]` ; `import_genome_facts(conn, blobs, *, document_id, attachments=None, actor_user=None, imported_at=None) -> ImportSummary` | D | job `import.genome_facts` |
| | `list_findings(conn, profile_id, *, gate, category=None, gene=None, min_evidence="E2", as_of=None) -> Findings` | D | E, F, API |
| | `lookup_variant(conn, blobs, profile_id, *, gate, rsid=None, hgvs=None, region=None, cursor=None) -> dict` | D | F |
| | `pgx_for_drug(conn, profile_id, *, cpic_drug_id, as_of=None) -> list[dict]` | D | E |
| | `is_sensitive_gene(gene) -> bool` ; constants `CALL_STATES`, `CATEGORIES`, `EVIDENCE` (final) | D | E, F |
| `state` | `canonical_json(obj) -> bytes` (RFC 8785) ; `rules_version() -> str` | E | F, J |
| | `build_state(conn, profile_id, *, as_of) -> dict` (body with `state_hash`) | E | F, API, J |
| | `render(body, *, lang, units="as_reported", identity_mode="pseudonymous") -> dict` | E | F, API |
| | `status_word(conn, profile_id, alias_group, *, as_of) -> "ok"\|"watch"\|"out"\|"stale"\|"nodata"` | E | A's API, H |
| | `check_drug(conn, profile_id, drug, *, scopes, lang="en", as_of=None) -> dict` (mcp.md 5.6 `data`) | E | F, API |
| | `resolve_drug(conn, name) -> {cpic_drug_id, name} \| None` | E | C, F |
| `mcp.envelope` | `envelope(*, profile, as_of, lang, identity_mode, data, **fields) -> dict` ; `estimate_tokens(obj)` ; `encode_cursor` / `decode_cursor` | F | F |
| `auth.sessions` | `resolve_session(conn, secret) -> Principal \| None` ; `create_session(conn, user_id, *, ip, user_agent, method="passkey", step_up=None) -> str` ; `require_step_up(conn, principal, max_age_seconds=300)` | G | foundation `principal.resolve`, G, export |
| `auth.passkeys` | `begin_registration` / `finish_registration` / `begin_login` / `finish_login` | G | G's API |
| `export` | `export_profile(conn, blobs, profile_id, out: BinaryIO, *, fmt="zip") -> dict` (streams the zip, returns the manifest; no blob copy) | G | G's API |
| `ops` | `backup(settings, dest) -> Path` ; `restore(settings, archive) -> dict` (same report as verify_restore) ; `verify_restore(settings, archive) -> dict` | J | CLI |

Additive changes the lanes made (every row above still works as written):

- **A** `Resolution` has `confidence` and `candidates[]` (`Candidate.partial`); reason `name_partial` (becomes a
  failing `loinc` check, reason `unmapped`). `resolve_analyte(..., household_id=None, text_result=False)`;
  `normalize_row(..., loinc=None, method=None)` keeps a received code when it is in the shipped cut;
  `parse_range(..., unit=None)`; `interpret(..., comparator=None)`; `RefRange.low_inclusive/high_inclusive/
  text_code`. New: `add_alias(conn, *, household_id, alias, alias_group, lang, added_by, unit=None,
  specimen=None, loinc=None) -> UUID`, `AliasError`, `group_units(alias_group)`, `display_name(alias_group, lang)`.
  `rederive` supersedes with `status_reason` `rederive <version>`; those rows are not corrections.
- **B** `ingest/pdf` is a package; `Word` has optional `font`, `size`, `bold`, `hidden`. Review kinds
  `candidate_row`, `amended_report`, `document_identity`, `pending_document` (images, scans, no table).
- **C** `wearables.series(conn, profile_id, metric, *, since, until, resolution="day"|"hour")`,
  `wearables.metric_info(key)`, `wearables.priority(...)`, `wearables.set_priority(...)`;
  `add_manual_observation(..., category=None, loinc=None, corrects=None, reason=None)`.
- **D** `pgx_for_drug(..., genes=None, drug_name=None)`; `gate_for(perm, *, surface, on, age_of_majority=18)`,
  `load_ref_sites(conn, rows, *, source)`, `GenomeImportError`, `GenomeQueryError(code, message, fix)`;
  `Findings.coverage`, `Findings.limitations`.
- **E** `build_state(..., known_at=None)` (a knowledge cut, not part of the hash); `render(..., scopes=None)`
  names left-out sections in `withheld.by_scope`; `state.dashboard(conn, pid, *, as_of, lang, scopes)`;
  `state.UnknownDrug`; `state.wording.lint(obj)`.
- **G** `begin_registration(..., user_name, purpose, data)`, `begin_login(..., user_id=None, session_id=None)`,
  `finish_login(..., purpose="login")`; `auth.deps.ProfilePerm(scope=, action=, manage=)`, `SessionOnly`, `StepUp`.
  `api/_deps.get_principal` runs `auth.csrf.check`, so every router gets the CSRF check.

`tests/test_contracts.py` imports every stub module, so a renamed module fails CI at once.

## 5. REST API

All routes are under `/api`, JSON, and resolve `{p}` as a profile slug, `me` or a uuid through
`core.access.require`. Errors are `{"detail": {"code": ..., "message": ..., ...}}`: 401 `unauthenticated`,
403 `forbidden` with the missing scope or action, 404 `profile_not_granted` (with `allowed` slugs) or
`unknown`, 409 for lifecycle and review conflicts, 415 `unsupported_upload`. A router module is
`ownhealth/api/<file>.py` with a module-level `router = APIRouter(...)`; `create_app` mounts every one
it finds, so adding a file is the whole registration. Modules starting with `_` are skipped.

| Owner | File | Route | Notes |
|---|---|---|---|
| foundation | core.py | `GET /me` | user, household, locale, dev flag |
| foundation | core.py | `GET /profiles` | `ProfileSummary[]` the caller can read, with role and scopes |
| foundation | core.py | `GET /sources/{src_id}` | mcp.md 5.7 shape; `unknown` 404 when not visible |
| foundation | review.py | `GET /review?status=&profile=&kind=&document_id=` | |
| foundation | review.py | `GET /review/{id}` ; `POST /review/{id}/accept` `{edits?}` ; `/reject` `{reason}` ; `/reopen` ; `/dismiss` `{reason?}` | action `review` |
| foundation | review.py | `POST /review/bulk-accept` `{document_id, min_row_conf}` | only items with no reasons |
| foundation | imports.py | `POST /imports` multipart `profile`, `files[]` | returns `{items: ImportItem[]}`; action `import` |
| foundation | imports.py | `GET /jobs?ids=1,2` | |
| — | app.py | `GET /healthz`, `GET /readyz` (outside `/api`) | |
| A | observations.py | `GET /profiles/{p}/observations?q=&loinc=&alias_group=&category=&from=&to=&units=&lang=` | `{items: ObservationOut[]}` |
| A | observations.py | `GET /profiles/{p}/trend?code=&window=P3Y&units=&lang=` | `Trend` (mcp.md 5.3 shape) |
| A | observations.py | `POST /aliases` | `{profile, alias, alias_group, lang?, loinc?, unit?, specimen?}`, action `review`; re-derives the household |
| B | documents.py | `GET /profiles/{p}/documents?q=&type=` ; `GET /documents/{id}` | |
| B | documents.py | `GET /documents/{id}/file` ; `/pages/{n}.png` ; `/pages/{n}/words` | words need `read:documents_text` for tokens |
| B | documents.py | `POST /documents/{id}/assign` `{profile}` ; `POST /documents/{id}/retract` `{reason}` | |
| C | records.py | `GET`/`POST /profiles/{p}/records/{kind}` ; `POST .../{kind}/{id}/correct` ; `.../{id}/retract` | |
| C | records.py | `POST /profiles/{p}/manual-observations` | |
| C | wearables.py | `GET /profiles/{p}/wearables/summary` ; `GET /profiles/{p}/wearables/{metric}?window=` | |
| D | genome.py | `GET /profiles/{p}/genome` ; `GET /profiles/{p}/genome/variant?rsid=` ; `POST /profiles/{p}/genome/consent` | consent needs step-up |
| E | status.py | `GET /profiles/{p}/status?as_of=` ; `GET /profiles/{p}/dashboard?lang=&as_of=` ; `GET /rules` | |
| E | drug_check.py | `GET /profiles/{p}/drug-check?drug=&lang=` | |
| F | mcp_usage.py | `GET /tokens/{id}/usage` | `/.well-known/oauth-protected-resource` lands in v0.2 |
| G | auth.py | passkey register/login, OIDC, `POST /auth/logout`, `POST /auth/step-up` | |
| G | tokens.py | `GET /tokens` ; `POST /tokens` (returns plaintext once) ; `DELETE /tokens/{id}` | |
| G | audit.py | `GET /audit?profile=&limit=&before=` | |
| G | family.py | `GET /household` ; `PUT /grants` ; `DELETE /grants?user_id=&profile=` ; `PUT /profiles/{p}/locks` ; `PUT /profiles/{p}/sensitive` ; `POST /household/profiles` ; `POST /household/invites` ; `DELETE /profiles/{p}?confirm=<slug>` ; `DELETE /account?confirm=<name>&delete_profile=` | |
| G | export.py | `POST /profiles/{p}/export` `{fmt}` | step-up; answers the zip itself |
| G | auth.py | `/auth/state`, `/auth/setup/{begin,finish,password}`, `/auth/login/{begin,finish}`, `/auth/recovery`, `/auth/password/login`, `/auth/step-up/{begin,finish}`, `/auth/passkeys` (GET, `/begin`, `/finish`, `DELETE /{id}`), `/auth/recovery-codes`, `/auth/invite/{code}`, `/auth/enroll/{begin,finish,password}`, `/auth/oidc/{login,callback}` | 403 `step_up_required`, 403 `csrf_failed` |
| H | timeline.py | `GET /profiles/{p}/timeline?types=&from=&to=&lang=` | |
| J | app-level | `GET /metrics` (Prometheus, outside `/api`) | |

The TypeScript shapes for every route are in `ui/src/lib/api/types.ts`; the client functions are in
`ui/src/lib/api/{core,health,manage}.ts`. A backend lane that changes a response shape updates the
type in the same PR.

## 6. Demo seed and hooks

`uv run ownhealth demo-seed [--force]`, or `OWNHEALTH_DEMO=1` at startup (seeds once, under an advisory
lock). The base seed (`demo/base.py`) creates household "Rivera (demo)": Alex, Sam and Lucía Rivera and
Carmen Ortega, grants as in `demo/family.py`, LabNorte PDF reports with bboxes, Alex's dashboard series
with the fixture src ids from mcp.md (`src_a12`, `src_a07`, `src_a05`, `src_9f3`, ...), medications,
conditions, a note, 90 days of wearable rollups and 7 open review items. `AS_OF = 2026-11-07`.

A lane adds demo data with `backend/src/ownhealth/demo/hooks/<lane>.py`:

```python
ORDER = 400            # A 100, B 200, C 300, D 400, E 500, F 600, G 700, J 900

def seed(conn, ctx: DemoContext) -> None:
    alex = ctx.family.profiles["alex"]
    ...                # use core functions; record ids other lanes cite in ctx.src_ids
```

Hooks run after the base seed, in `ORDER` then module name, inside the same transaction. A hook that
raises rolls back the whole seed, so `tests/test_demo.py` catches it. Never reuse a src id the base
seed already took (`ctx.src_ids` and `demo/base.py` list them).

## 7. Migrations

Plain numbered SQL in `backend/src/ownhealth/db/migrations/NNNN_name.sql`, applied in number order,
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

H and I have no migrations. Prefer using the 0001 tables; add columns only when the design needs them.
Lifecycle rule for every clinical table: only `status`, `status_reason` and `superseded_at` change after
insert (a trigger enforces it), and `audit_event` is append-only.

## 8. MCP tools

The server is `ownhealth.mcp.build_server(settings)`, streamable HTTP at `/mcp`, stateless, JSON
responses; protocol 2026-07-28 with 2025-11-25 accepted. Foundation ships a `ping` placeholder that lane F
removes. Full schemas and examples: `docs/design/mcp.md` section 5; the table below is the contract
summary every lane builds against.

| Tool | Scope | Input (beyond `profile`) | Output `data` | Budget | Backed by |
|---|---|---|---|---|---|
| `get_health_status` | `read:summary` (+ domain scopes widen sections) | `as_of` | `rules_version`, `state_hash`, `flags[]`, `medications[]`, `supplements[]`, `active_conditions[]`, `never_measured[]`, `stale[]`, `pending[]`, `corrections[]`, `notes[]`, `data_freshness{}` | < 2 KB | E `build_state` + `render` |
| `search_observations` | `read:labs` / `read:vitals` / `read:wearables` by category | `query` or `loinc`, `category`, `from`, `to`, `flagged_only`, `group_by`, `detail`, `cursor`, `as_of` | `results[]` `{name, code, value, unit, canonical, range, flag, date, lab, draw_id, src}`, `resolved` | 4k, max 15k | core `list_observations`, A |
| `get_trend` | as above | `code`, `window` (`P\d+[DMY]`/`all`), `resolution`, `as_of` | `name, codes, canonical_unit, resolution, coverage_pct, points[], stats, annotations[]` | 2k, max 6k | A, C rollups |
| `get_genome_findings` | `read:genome` (+ `read:sensitive` and the agents switch for sensitive) | `category`, `gene`, `min_evidence`, `detail`, `cursor`, `as_of` | `assay`, `findings[]` (each with `call_state`) ; `not_assessed[]` ; `db_versions` | 4k, max 15k | D `list_findings` |
| `lookup_variant` | `read:genome_raw` | one of `rsid`, `hgvs`, `region`; `cursor` | `query`, `variant`, `calls[]` (`call_state` always), `annotations`, `cited_by_findings`; `quota` | ~500 per position | D `lookup_variant` |
| `check_drug` | any of `read:genome`, `read:records`, `read:labs` (missing parts in `withheld.by_scope`) | `drug` | `drug`, `pgx[]`, `phenoconversion[]`, `current_medications_relevant[]`, `relevant_labs[]`, `guidance[]`, `not_assessed[]` | 2k, max 4k | E `check_drug`, D `pgx_for_drug` |
| `get_source` | scope of the underlying fact; text needs `read:documents_text` | `src_id`, `include_text` | `src_id, status, document, location, printed, derived, extraction, crop_url, text` | 800, 4k with text | core `resolve_src`, B `page_words` |
| `propose` | `write:propose` + read on the profile | `kind` (`observations`, `note`, `finding_status`, `medication`), `payload`, `based_on[]` | `proposal_id, status: queued_for_review, kind, rows[]/..., review_url` | 1k | core `create_item` (kind `agent_proposal`), A, B `ground_value` |

Rules every tool follows (mcp.md 3, 9):

- Envelope fields: `profile, as_of, generated_at, lang, identity_mode, data, evidence, limitations,
  not_assessed, withheld{sensitive, by_scope, minor_rule}, db_versions, next_cursor,
  budget{tokens_est, truncated}, notice`, plus a compact markdown text block with the same facts.
- `tools/list` shows only tools whose scope the token holds. All tools: `openWorldHint: false`; all but
  `propose` are `readOnlyHint: true, idempotentHint: true`.
- Every fact carries `src`. Ids are stable and resolve through `get_source`.
- Free text is `{"trust": "untrusted_text", "origin": <pdf_text_layer|ocr|email_subject|fhir_note|
  agent_note|person_note>, "text": ...}` via `mcp.envelope.untrusted`.
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
- `pnpm run check` (svelte-check, warnings fail) and `pnpm build` (into `backend/src/ownhealth/static`)
  must pass.
