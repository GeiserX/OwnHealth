-- 1120, v0.3 genome: the ClinVar diff (docs/design/genomics.md section 9).
--
-- clinvar_carried is the last ClinVar release compared for one genome dataset, cut to the records the
-- person carries an alt allele of; the next release is diffed against it. genome_event keeps every
-- change found, with both releases; `alerted` says whether it became a genome_reinterpretation review
-- item. gene, sensitive, adult_onset and carrier are kept so the reader's gate (sensitive switch,
-- read:sensitive, the minor rule) decides at read time whether the gene may be named.
-- Neither table has a lifecycle trigger: a snapshot is replaced, an event never changes. Both go
-- with their dataset (on delete cascade), so deleting a profile is never blocked by them.

alter table source drop constraint source_kind_check;
alter table source add constraint source_kind_check check (kind in ('email', 'folder', 'paperless', 'share',
  'shortcut', 'health_auto_export', 'gadgetbridge', 'nightscout', 'clinvar'));

create table clinvar_carried (
  dataset_id uuid not null references genome_dataset on delete cascade,
  release_date date not null,          -- the ClinVar VCF's ##fileDate
  variation_id bigint not null,        -- ClinVar VariationID (the VCF ID column)
  gene text,                           -- first symbol of GENEINFO
  significance text not null,          -- CLNSIG as ClinVar wrote it
  stars smallint not null check (stars between 0 and 4),   -- from CLNREVSTAT
  primary key (dataset_id, variation_id));

create table genome_event (
  id uuid primary key default gen_random_uuid(),
  profile_id uuid not null references profile,
  dataset_id uuid not null references genome_dataset on delete cascade,
  variation_id bigint not null,
  kind text not null check (kind in ('upgraded_to_plp', 'downgraded_from_plp', 'stars_changed',
        'became_conflicting', 'vus_resolved', 'significance_changed', 'new_assertion_on_carried_variant')),
  gene text,
  sensitive boolean not null,
  adult_onset boolean not null,
  carrier boolean not null,
  old jsonb not null,                  -- {class, significance, stars, fact_id} before; {} when nothing was reported
  new jsonb not null,                  -- {class, significance, stars} in release_to
  release_from text,                   -- previous snapshot's release, else the facts file's db_versions.clinvar
  release_to text not null,
  alerted boolean not null,
  created_at timestamptz not null default now());
create index on genome_event (profile_id, created_at desc);
create index on genome_event (dataset_id);
