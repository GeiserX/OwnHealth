-- Lane B: a document nobody could read (image, scanned page, no lab table found) waits in the review
-- queue as one `pending_document` item for agent-assisted extraction (MCP propose) or manual entry.
alter table review_item drop constraint review_item_kind_check;
alter table review_item add constraint review_item_kind_check check (kind in ('candidate_row', 'agent_proposal',
  'fhir_unmapped', 'document_identity', 'amended_report', 'rederive_diff', 'genome_reinterpretation', 'job_failed',
  'restore_check_failed', 'pending_document'));
