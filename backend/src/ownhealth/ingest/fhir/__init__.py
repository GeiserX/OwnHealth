"""Lane C: FHIR R4 Bundle / IPS / NDJSON import (ingestion.md 3.3, architecture.md 9). Job kind
`import.fhir_bundle`.

Accepts a Bundle (`collection`, `document` with a Composition as in IPS, `searchset`, `transaction`),
NDJSON (one resource or Bundle per line) or a single resource. Each resource keeps its received JSON
in `fhir` and gets `src(kind='fhir_import', import_ref='<Bundle.id>/<resourceType>/<id>')`.

- Observations go through `core.review.route_fact`: a valid LOINC code (format and check digit) and a
  UCUM unit, with the Patient matching the profile, auto-accept; anything else waits in review.
- Condition, AllergyIntolerance, Immunization, Procedure, MedicationStatement and MedicationRequest
  become records under the same auto-accept rule; held ones wait in review as `fhir_unmapped` with
  the projected record in the payload, and accepting creates it.
- Other resource types are stored as received (the bundle is the blob) and listed in review as
  `fhir_unmapped`, "stored, not projected". Nothing is dropped.
- A resource already imported with the same JSON is skipped; one that changed upstream goes to review
  with reason `conflict`, and accepting it supersedes the old version (records and observations).
  FHIR logical ids are unique only per server, so "the same resource" means the same type/id from the
  same issuer: the Patient identifier, else the server base of absolute fullUrls, else the id of the
  embedded Patient (`issuer_key`). A file with no issuer is matched only against itself; a resource
  from another issuer that reuses an id is imported as a separate fact.
"""

from __future__ import annotations

import hashlib
import json
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from datetime import UTC, datetime
from uuid import UUID

from sqlalchemy import Connection, text

from ownhealth import normalize
from ownhealth.blobs import BlobStore
from ownhealth.core.acceptance import Check, auto_accept
from ownhealth.core.access import Profile, get_profile
from ownhealth.core.observations import ObservationIn
from ownhealth.core.provenance import SrcIn
from ownhealth.core.records import MODELS, TABLES, add_record, supersede_record
from ownhealth.core.review import ReviewError, ReviewItemIn, create_item, register_accept_handler, route_fact
from ownhealth.ingest import ImportSummary
from ownhealth.ingest.fhir import mapping

# Normalizer reasons about resolving the printed name; a valid received LOINC code makes them moot.
NAME_RESOLUTION = frozenset({"unmapped", "loinc_unknown"})


@dataclass(frozen=True)
class Entry:
    pointer: str  # where the resource sits in the file: /entry/3/resource, L12, /
    resource: dict
    full_url: str | None = None


def read_entries(data: bytes, *, ndjson: bool) -> tuple[str | None, list[Entry]]:
    """(Bundle.id, entries) of a JSON file or an NDJSON file."""
    if ndjson:
        out: list[Entry] = []
        for n, line in enumerate(data.splitlines(), start=1):
            if line.strip():
                out.extend(_expand(json.loads(line), f"L{n}"))
        return None, out
    doc = json.loads(data)
    bundle_id = doc.get("id") if doc.get("resourceType") == "Bundle" else None
    return bundle_id, _expand(doc, "")


def _expand(res: dict, prefix: str) -> list[Entry]:
    if not isinstance(res, dict) or "resourceType" not in res:
        raise ValueError(f"not a FHIR resource at {prefix or '/'}")
    if res["resourceType"] != "Bundle":
        return [Entry(prefix or "/", res)]
    return [
        Entry(f"{prefix}/entry/{i}/resource", e["resource"], e.get("fullUrl"))
        for i, e in enumerate(res.get("entry") or [])
        if isinstance(e.get("resource"), dict)
    ]


def import_fhir(conn: Connection, blobs: BlobStore, *, document_id: UUID) -> ImportSummary:
    doc = (
        conn.execute(
            text(
                "select profile_id, encode(blob_sha256, 'hex') as sha, mime, filename, flags from document"
                " where id = :d"
            ),
            {"d": document_id},
        )
        .mappings()
        .one()
    )
    done = (doc["flags"] or {}).get("import")
    if done:  # the job ran before (a retry, or the same bytes uploaded again)
        return ImportSummary(**{k: done[k] for k in ("document_id", "accepted", "queued", "skipped", "reasons")})
    if doc["profile_id"] is None:
        raise ValueError("assign the document to a profile before importing it")
    profile = get_profile(conn, doc["profile_id"])
    ndjson = doc["mime"] == "application/fhir+ndjson" or (doc["filename"] or "").lower().endswith(".ndjson")
    bundle_id, entries = read_entries(blobs.get_bytes(doc["sha"]), ndjson=ndjson)
    summary, identity = import_resources(
        conn, entries, profile=profile, document_id=document_id, bundle_key=bundle_id or f"doc-{doc['sha'][:12]}"
    )
    mark_imported(conn, document_id, summary, identity)
    return summary


def mark_imported(conn: Connection, document_id: UUID, summary: ImportSummary, identity: str | None) -> None:
    conn.execute(
        text(
            "update document set flags = flags || jsonb_build_object('import', cast(:s as jsonb)),"
            " identity_match = coalesce(:i, identity_match) where id = :d"
        ),
        {"s": json.dumps(summary.as_dict()), "i": identity, "d": document_id},
    )


# ----------------------------------------------------------------- importer ----


def import_resources(
    conn: Connection,
    entries: Iterable[Entry],
    *,
    profile: Profile,
    document_id: UUID,
    bundle_key: str,
    identity: str | None = None,
    label: str = "FHIR",
) -> tuple[ImportSummary, str]:
    """Import resources into one profile. `identity` is computed from the Patient when not given
    (Apple Health passes the result of its own birth-date check). Returns (summary, identity)."""
    entries = list(entries)
    index: dict[str, dict] = {}
    for e in entries:
        r = e.resource
        if r.get("id"):
            index[f"{r['resourceType']}/{r['id']}"] = r
        if e.full_url:
            index[e.full_url] = r

    def resolve(ref: str | None) -> dict | None:
        if not ref:
            return None
        return index.get(ref) or index.get("/".join(ref.rstrip("/").split("/")[-2:]))

    if identity is None:
        patient = None
        for e in entries:
            if e.resource["resourceType"] == "Composition":
                patient = resolve((e.resource.get("subject") or {}).get("reference"))
                break
        patient = patient or next((e.resource for e in entries if e.resource["resourceType"] == "Patient"), None)
        identity = mapping.match_patient(patient, profile.display_name, profile.birth_date)

    report_keys: dict[str, str] = {}
    for e in entries:
        r = e.resource
        if r["resourceType"] == "DiagnosticReport":
            for res in r.get("result") or []:
                target = resolve(res.get("reference"))
                if target and target.get("id"):
                    report_keys[f"Observation/{target['id']}"] = f"{bundle_key}/DiagnosticReport/{r.get('id')}"

    issuer = issuer_key(entries)
    conn.execute(
        text("update document set flags = flags || jsonb_build_object('fhir_issuer', cast(:i as text)) where id = :d"),
        {"i": issuer, "d": document_id},
    )
    run = _Run(conn, profile, document_id, bundle_key, identity, label, resolve, report_keys, issuer)
    for e in entries:
        run.one(e)
    return run.summary, identity


def issuer_key(entries: list[Entry]) -> str | None:
    """Who issued these resource ids, as a short hash, or None when the file does not say."""
    patient = next((e.resource for e in entries if e.resource.get("resourceType") == "Patient"), None)
    key = None
    for ident in (patient or {}).get("identifier") or []:
        if isinstance(ident, dict) and ident.get("value"):
            key = f"identifier:{ident.get('system') or ''}|{ident['value']}"
            break
    if key is None:
        for e in entries:
            url, r = e.full_url or "", e.resource
            tail = f"/{r.get('resourceType')}/{r.get('id')}"
            if url.startswith(("http://", "https://")) and url.endswith(tail):
                key = "base:" + url[: -len(tail)].rstrip("/")
                break
    if key is None and patient and patient.get("id"):
        key = f"patient:{patient['id']}"
    return hashlib.sha256(key.encode()).hexdigest()[:16] if key else None


class _Run:
    def __init__(self, conn, profile, document_id, bundle_key, identity, label, resolve, report_keys, issuer=None):
        self.issuer = issuer
        self.conn = conn
        self.profile = profile
        self.document_id = document_id
        self.bundle_key = bundle_key
        self.identity = identity
        self.label = label
        self.resolve: Callable[[str | None], dict | None] = resolve
        self.report_keys = report_keys
        self.summary = ImportSummary(document_id=str(document_id))

    # -- helpers

    def _count(self, reasons: Iterable[str]) -> None:
        for r in reasons:
            self.summary.reasons[r] = self.summary.reasons.get(r, 0) + 1

    def _src(self, ref: str, when: datetime | None) -> SrcIn:
        day = when.date().isoformat() if when else "no date"
        return SrcIn(
            kind="fhir_import",
            profile_id=self.profile.id,
            document_id=self.document_id,
            import_ref=ref,
            label=f"{self.label} · {ref.split('/', 1)[-1].split('/')[0]} · {day}",
        )

    @staticmethod
    def _like(tail: str) -> str:
        return "%/" + tail.replace("\\", "\\\\").replace("%", "\\%").replace("_", "\\_")

    def _existing(self, table: str, tail: str) -> list[dict]:
        """Visible facts of this profile imported before from a resource with the same type/id, with
        whether they came from the same issuer (or this same file)."""
        return [
            dict(r)
            for r in self.conn.execute(
                text(
                    f"select f.id, f.fhir, (s.document_id = :doc or (cast(:iss as text) is not null"
                    f" and d.flags->>'fhir_issuer' = :iss)) as same_issuer"
                    f" from {table} f join src s on s.id = f.src_id left join document d on d.id = s.document_id"
                    " where f.profile_id = :p and s.kind = 'fhir_import' and f.status in ('current', 'provisional')"
                    " and s.import_ref like :l"
                ),
                {"p": self.profile.id, "l": self._like(tail), "doc": self.document_id, "iss": self.issuer},
            ).mappings()
        ]

    def _pending(self, tail: str) -> bool:
        """An open review item of the same issuer already holds this type/id."""
        return (
            self.conn.execute(
                text(
                    "select 1 from review_item ri left join document d on d.id = ri.document_id"
                    " where ri.profile_id = :p and ri.status = 'open'"
                    " and ri.kind in ('candidate_row', 'fhir_unmapped') and ri.payload->'src'->>'import_ref' like :l"
                    " and (ri.document_id = :doc or (cast(:iss as text) is not null"
                    " and d.flags->>'fhir_issuer' = :iss)) limit 1"
                ),
                {"p": self.profile.id, "l": self._like(tail), "doc": self.document_id, "iss": self.issuer},
            ).first()
            is not None
        )

    def _dedupe(self, table: str, tail: str, resource: dict, checks: list[Check]) -> tuple[bool, UUID | None]:
        """(skip, superseded candidate). Same JSON imported before: skip. Changed at the same issuer: a
        conflict check naming the version it would replace. Another issuer's id: a separate fact."""
        if self._pending(tail):
            return True, None
        existing = self._existing(table, tail)
        if any(row["fhir"] == resource for row in existing):
            return True, None
        for row in existing:
            if row["same_issuer"]:
                checks.append(
                    Check(name="upstream_version", ok=False, reason="conflict", detail="changed since import")
                )
                return False, row["id"]
        return False, None

    def _queue_unmapped(self, e: Entry, note: str, ref: str) -> None:
        r = e.resource
        create_item(
            self.conn,
            ReviewItemIn(
                kind="fhir_unmapped",
                household_id=self.profile.household_id,
                profile_id=self.profile.id,
                document_id=self.document_id,
                reasons=["unmapped"],
                payload={
                    "projection": None,
                    "note": note,
                    "resource_type": r["resourceType"],
                    "json_pointer": e.pointer,
                    "src": {"import_ref": ref},
                    "resource": r,
                },
            ),
        )
        self.summary.queued += 1
        self._count(["unmapped"])

    # -- one resource

    def one(self, e: Entry) -> None:
        r = e.resource
        rtype = r["resourceType"]
        if rtype in mapping.SUPPORTING or rtype == "DiagnosticReport":
            return  # context for the others; kept in the stored bundle
        rid = r.get("id") or "sha-" + hashlib.sha256(json.dumps(r, sort_keys=True).encode()).hexdigest()[:16]
        tail = f"{rtype}/{rid}"
        ref = f"{self.bundle_key}/{tail}"
        if rtype == "Observation":
            m = mapping.observation(r)
        elif rtype == "Condition":
            m = mapping.condition(r, self.profile.id)
        elif rtype == "AllergyIntolerance":
            m = mapping.allergy(r, self.profile.id)
        elif rtype == "Immunization":
            m = mapping.immunization(r, self.profile.id)
        elif rtype == "Procedure":
            m = mapping.procedure(r, self.profile.id)
        elif rtype in ("MedicationStatement", "MedicationRequest"):
            m = mapping.medication(r, self.profile.id, self.resolve)
        else:
            m = None
        if m is None:
            if not self._pending(tail):
                self._queue_unmapped(e, "stored, not projected", ref)
            else:
                self.summary.skipped += 1
                self._count(["duplicate"])
            return
        if m.kind == "skip":
            self.summary.skipped += 1
            self._count([f"skip:{m.skip_reason}"])
            return
        if m.unmapped_note:
            self._queue_unmapped(e, m.unmapped_note, ref)
            return
        if m.kind == "observation":
            for row in m.observations:
                self._observation(e, row, tail + row.suffix, ref + row.suffix, r)
        else:
            self._record(e, m, tail, ref)

    def _observation(self, e: Entry, row: mapping.ObsRow, tail: str, ref: str, resource: dict) -> None:
        checks = list(row.checks)
        skip, previous = self._dedupe("observation", tail, resource, checks)
        if skip:
            self.summary.skipped += 1
            self._count(["duplicate"])
            return
        obs, norm_checks = self._normalize(row, resource, tail.split("#")[0])
        checks += [c for c in norm_checks if c.name not in {k.name for k in checks}]
        allowance = "structured_valid" if all(c.ok for c in checks) else "structured_partial"
        result = route_fact(
            self.conn,
            observation=obs,
            src=self._src(ref, row.effective_at),
            checks=checks,
            row_conf=1.0,
            allowance=allowance,
            identity=self.identity,
            provisional=resource.get("status") == "preliminary",
            extra_payload={
                "json_pointer": e.pointer,
                "resource_type": "Observation",
                "supersedes": str(previous) if previous else None,
            },
        )
        if result.state == "auto_accepted":
            self.summary.accepted += 1
        else:
            self.summary.queued += 1
            self._count(result.reasons)

    def _normalize(self, row: mapping.ObsRow, resource: dict, tail: str) -> tuple[ObservationIn, list[Check]]:
        """Received codes win; lane A fills the derived layer (alias group, canonical unit) when it can."""
        when = row.effective_at or datetime.now(UTC)
        fields = {
            "profile_id": self.profile.id,
            "category": row.category,
            "source_kind": "clinical",
            "printed_name": row.printed_name,
            "printed_value": row.printed_value,
            "printed_unit": row.printed_unit,
            "printed_ref_range": row.printed_ref_range,
            "value_num": row.value_num,
            "value_cmp": row.value_cmp,
            "value_text": row.value_text,
            "value_code": row.value_code,
            "ref_low": row.ref_low,
            "ref_high": row.ref_high,
            "effective_at": when,
            "document_id": self.document_id,
            "report_key": self.report_keys.get(tail),
            "sensitivity": mapping.sensitivity(resource),
            "fhir": resource,
        }
        received = {k: v for k, v in (("loinc", row.loinc), ("unit_ucum", row.unit_ucum)) if v}
        if row.interpretation:
            received["interpretation"] = row.interpretation
        try:
            # the received LOINC and UCUM code are truth like the printed text: they drive the derived
            # layer (alias group, canonical value), not only the columns they land in
            n = normalize.normalize_row(
                self.conn,
                profile_id=self.profile.id,
                printed_name=row.printed_name,
                printed_value=row.printed_value,
                printed_unit=row.unit_ucum or row.printed_unit,
                printed_range=row.printed_ref_range,
                printed_flag=row.interpretation,
                effective_at=when,
                lang="es" if (resource.get("language") or "").startswith("es") else "en",
                loinc=row.loinc if mapping.loinc_valid(row.loinc) else None,
                document_id=self.document_id,
            )
        except NotImplementedError:  # lane A not landed: keep the received codes as the derived layer
            group = (
                f"loinc:{row.loinc}"
                if mapping.loinc_valid(row.loinc)
                else "name:" + "_".join(sorted(mapping.name_tokens(row.printed_name)))
            )
            obs = ObservationIn(
                **fields,
                **received,
                alias_group=group,
                value_canonical=row.value_num if row.unit_ucum else None,
                unit_canonical=row.unit_ucum,
                derivation_version=normalize.DERIVATION_VERSION,
            )
            return obs, []
        derived = n.observation.model_dump(exclude=set(fields) | set(received))
        checks = list(n.checks)
        if mapping.loinc_valid(row.loinc):  # the received code answers what name resolution could not
            checks = [c for c in checks if c.ok or c.reason not in NAME_RESOLUTION]
        return ObservationIn(**derived, **fields, **received), checks

    def _record(self, e: Entry, m: mapping.Mapped, tail: str, ref: str) -> None:
        checks = list(m.checks)
        skip, previous = self._dedupe(TABLES[m.record_kind], tail, e.resource, checks)
        if skip:
            self.summary.skipped += 1
            self._count(["duplicate"])
            return
        allowance = "structured_valid" if all(c.ok for c in checks) else "structured_partial"
        decision = auto_accept(1.0, checks, allowance, self.identity)
        src = self._src(ref, m.record.effective_at)
        if decision.auto:
            add_record(self.conn, m.record, src=src, accepted_by="import")
            self.summary.accepted += 1
            return
        create_item(
            self.conn,
            ReviewItemIn(
                kind="fhir_unmapped",
                household_id=self.profile.household_id,
                profile_id=self.profile.id,
                document_id=self.document_id,
                reasons=decision.reasons,
                row_conf=1.0,
                payload={
                    "projection": "record",
                    "record_kind": m.record_kind,
                    "record": m.record.model_dump(mode="json"),
                    "src": src.model_dump(mode="json"),
                    "supersedes": str(previous) if previous else None,
                    "checks": [c.model_dump() for c in checks],
                    "identity": self.identity,
                    "why_not": decision.why_not,
                    "resource_type": e.resource["resourceType"],
                    "json_pointer": e.pointer,
                },
            ),
        )
        self.summary.queued += 1
        self._count(decision.reasons)


# ----------------------------------------------------------- review accept ----


@register_accept_handler("fhir_unmapped")
def _accept_fhir_item(conn: Connection, item: dict, user_id: UUID, edits: dict | None) -> list[UUID]:
    """A held record becomes a record (a correction when it changed upstream). A resource we do not
    project cannot be accepted: it stays stored, and the person dismisses the item."""
    payload = item["payload"]
    if payload.get("projection") != "record":
        raise ReviewError("stored, not projected: this resource type has no table yet; dismiss the item")
    kind = payload["record_kind"]
    record = MODELS[kind].model_validate({**payload["record"], **(edits or {})})
    src = SrcIn.model_validate({**payload["src"], "actor_kind": "user", "actor_id": str(user_id)})
    if payload.get("supersedes"):
        committed = supersede_record(
            conn, UUID(payload["supersedes"]), record, src=src, accepted_by="person", reason="updated at the source"
        )
    else:
        committed = add_record(conn, record, src=src, accepted_by="person")
    return [committed.id]
