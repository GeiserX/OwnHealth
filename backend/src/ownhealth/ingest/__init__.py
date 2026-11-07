"""Importers. Each one turns a stored document into facts routed through `core.review.route_fact`.

Lane B owns `ingest/pdf`, lane C owns `ingest/fhir`, `ingest/apple_health` and `ingest/manual`.
Job handlers live in `ownhealth/jobs/kinds/<kind>.py` and are registered with `@jobs.handler`.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class ImportSummary:
    """What an import job returns (stored in `job.result` and shown in the import screen)."""

    document_id: str
    accepted: int = 0
    queued: int = 0  # review items created
    skipped: int = 0  # duplicates
    reasons: dict[str, int] = field(default_factory=dict)  # reason code -> count

    def as_dict(self) -> dict:
        return {
            "document_id": self.document_id,
            "accepted": self.accepted,
            "queued": self.queued,
            "skipped": self.skipped,
            "reasons": self.reasons,
        }
