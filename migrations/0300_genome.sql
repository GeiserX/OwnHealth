-- Lane D, genome (docs/design/genomics.md). Raw genome files stay in the blob store; these columns
-- only point at them by sha256. Nothing here holds a genotype except the small identity fingerprint.

alter table genome_dataset
  add column document_id uuid references document,       -- the uploaded genome-facts.json
  add column gvcf_blob bytea,                             -- gVCF with reference blocks (WGS), and its index
  add column gvcf_tbi_blob bytea,
  add column sites_blob bytea,                            -- chip: every probed position (makes not_on_array exact)
  add column sites_tbi_blob bytea,
  add column assay_detail jsonb not null default '{}',    -- assay object of the facts file
  add column producer jsonb not null default '{}',        -- provenance object minus db_versions
  add column subject jsonb not null default '{}',         -- sample_id, declared sex, karyotype, fingerprint
  add column qc jsonb not null default '{}',
  add column superseded_at timestamptz;
create index on genome_dataset (profile_id, status);

-- Curated rsID -> GRCh38 position index (dbSNP, public data; no personal data). Loaded from a shipped
-- file; an rsID outside it answers rsid_unknown, never a remote lookup.
create table genome_ref_site (
  rsid text not null check (rsid ~ '^rs[0-9]+$'),
  chrom text not null,
  pos integer not null check (pos >= 1),
  ref text not null,
  alts text[] not null default '{}',
  gene text,
  source text not null,                                   -- 'dbsnp-b157', 'test', ...
  primary key (rsid, chrom, pos));
create index on genome_ref_site (chrom, pos);
