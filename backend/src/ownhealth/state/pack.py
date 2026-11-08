"""The rule pack: shipped YAML in `ownhealth/rules/pack/`, plus an optional override directory.

An override file replaces the shipped file with the same name (or adds a new body system). With any
override the version becomes `ownhealth-rules 0.1.0+local.<sha8>`, so a changed pack never passes
for the shipped one. `sha256` is the hash of the canonical JSON of every loaded file.
"""

from __future__ import annotations

import hashlib
import json
import os
from dataclasses import dataclass, field
from decimal import Decimal
from functools import lru_cache
from pathlib import Path

import yaml

from ownhealth.state.canonical import canonical_json

RULES_DIR = Path(__file__).resolve().parent.parent / "rules"
PACK_DIR = RULES_DIR / "pack"
CPIC_SNAPSHOT = RULES_DIR / "cpic" / "snapshot.json"
OVERRIDE_ENV = "OWNHEALTH_RULES_OVERRIDE_DIR"
RULE_KINDS = ("lab_range", "recency", "rcv", "trend", "never_measured", "gene_drug", "gene_lab")
NON_SYSTEM_FILES = {
    "pack.yaml",
    "analytes.yaml",
    "screening.yaml",
    "pgx.yaml",
    "pgx_phenoconversion.yaml",
    "drugs.yaml",
    "drug_labs.yaml",
    "templates.yaml",
}


class PackError(ValueError):
    pass


@dataclass(frozen=True)
class System:
    key: str
    file: str
    order: int
    names: dict
    source: str  # labs | wearables
    markers: tuple  # alias groups (labs) or marker dicts (wearables)


@dataclass
class Pack:
    version: str
    sha256: str
    files: dict[str, dict]
    systems: list[System]
    rules: list[dict]  # every rule, any file, with "file" set
    analytes: dict[str, dict]
    templates: dict[str, dict]
    drugs: dict[str, list[str]]
    drug_labs: dict[str, list[dict]]
    phenoconversion: dict
    dashboard: dict
    limitations: list[str]
    order: list[str]
    loinc_to_group: dict[str, str] = field(default_factory=dict)

    def rules_of(self, kind: str) -> list[dict]:
        return [r for r in self.rules if r["kind"] == kind]

    def rule(self, rule_id: str) -> dict | None:
        return next((r for r in self.rules if r["id"] == rule_id), None)

    def name(self, group: str, lang: str) -> str:
        a = self.analytes.get(group)
        return (a.get(lang) or a.get("en")) if a else group

    def text(self, key: str, lang: str, **params) -> str:
        # A count of one takes the `.one` template when there is one ("1 day ago", "hace 1 día").
        t = (self.templates.get(f"{key}.one") if params.get("n") == 1 else None) or self.templates.get(key)
        if t is None:
            raise PackError(f"no template {key}")
        return (t.get(lang) or t["en"]).format(**params)


def _read_dir(path: Path) -> dict[str, dict]:
    return {p.name: yaml.safe_load(p.read_text(encoding="utf-8")) or {} for p in sorted(path.glob("*.yaml"))}


def _check(pack: Pack) -> None:
    ids: set[str] = set()
    for r in pack.rules:
        if not r.get("id") or r["id"] in ids:
            raise PackError(f"rule id missing or repeated: {r.get('id')!r} in {r.get('file')}")
        ids.add(r["id"])
        if r.get("kind") not in RULE_KINDS:
            raise PackError(f"rule {r['id']}: unknown kind {r.get('kind')!r}")
        if not str(r.get("cite", "")).startswith("https://"):
            raise PackError(f"rule {r['id']}: every rule needs an https cite")
    for key in pack.templates:
        if "en" not in pack.templates[key] or "es" not in pack.templates[key]:
            raise PackError(f"template {key} needs en and es")


def build_pack(files: dict[str, dict], overridden: bool) -> Pack:
    head = files.get("pack.yaml") or {}
    if head.get("pack") != "ownhealth-rules" or not head.get("version"):
        raise PackError("pack.yaml must name pack ownhealth-rules and a version")
    sha = hashlib.sha256(canonical_json(files)).hexdigest()
    version = f"ownhealth-rules {head['version']}" + (f"+local.{sha[:8]}" if overridden else "")
    systems: list[System] = []
    rules: list[dict] = [dict(r, file="pack.yaml") for r in head.get("rules", [])]
    for name, doc in files.items():
        if name in NON_SYSTEM_FILES:
            if name in ("screening.yaml", "pgx.yaml"):
                rules += [dict(r, file=name) for r in doc.get("rules", [])]
            continue
        if "system" not in doc:
            raise PackError(f"{name}: not a known pack file and has no `system`")
        systems.append(
            System(
                key=doc["system"],
                file=name,
                order=int(doc.get("order", 99)),
                names=doc.get("names", {}),
                source=doc.get("source", "labs"),
                markers=tuple(doc.get("markers", [])),
            )
        )
        rules += [dict(r, file=name, system=doc["system"]) for r in doc.get("rules", [])]
    systems.sort(key=lambda s: (s.order, s.key))
    analytes = (files.get("analytes.yaml") or {}).get("analytes", {})
    pack = Pack(
        version=version,
        sha256=sha,
        files=files,
        systems=systems,
        rules=rules,
        analytes=analytes,
        templates=(files.get("templates.yaml") or {}).get("templates", {}),
        drugs=(files.get("drugs.yaml") or {}).get("drugs", {}),
        drug_labs=(files.get("drug_labs.yaml") or {}).get("drug_labs", {}),
        phenoconversion=files.get("pgx_phenoconversion.yaml") or {},
        dashboard=head.get("dashboard", {}),
        limitations=list(head.get("limitations", [])),
        order=list(head.get("order", [])),
    )
    for group, a in analytes.items():
        for code in a.get("loinc", []):
            pack.loinc_to_group.setdefault(str(code), group)
    _check(pack)
    return pack


@lru_cache(maxsize=4)
def _load(override_dir: str | None) -> Pack:
    files = _read_dir(PACK_DIR)
    overridden = False
    if override_dir:
        extra = _read_dir(Path(override_dir))
        overridden = bool(extra)
        files.update(extra)
    return build_pack(dict(sorted(files.items())), overridden)


def load_pack() -> Pack:
    return _load(os.environ.get(OVERRIDE_ENV) or None)


@lru_cache(maxsize=1)
def cpic_snapshot() -> dict:
    return json.loads(CPIC_SNAPSHOT.read_text(encoding="utf-8"))


def dec(value) -> Decimal | None:
    return None if value is None else Decimal(str(value))
