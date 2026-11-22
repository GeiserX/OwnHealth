-- 1291, v1.0 Connected: CGM CSV exports are wearables documents.
--
-- A LibreView or Dexcom Clarity CSV export holds every glucose reading of its sensors. Migration 1221
-- made doc_type 'wearables' (ownhealth_core::documents::WEARABLES_DOC_TYPE), which needs read:wearables
-- to be listed, opened or downloaded, and the CGM importer now stores its file with it. The CGM exports
-- stored before this file were doc_type 'other': each one carries flags.cgm_csv from its first import,
-- so they move here. It numbers above 1221, whose CHECK allows 'wearables'. Running it twice changes
-- nothing more.
update document set doc_type = 'wearables'
 where doc_type is distinct from 'wearables'
   and flags ? 'cgm_csv';
