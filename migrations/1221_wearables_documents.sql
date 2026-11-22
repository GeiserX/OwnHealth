-- 1221, v1.0 Connected (wearables block 1220-1229): raw wearables exports are their own document type.
--
-- A FIT activity file and a Gadgetbridge database hold every reading of a device, and a FIT file held
-- the route. As doc_type 'other' they were listed, opened and downloaded with read:records, so a member
-- the server refuses wearables to could read them. doc_type 'wearables' (ownhealth_core::documents::
-- WEARABLES_DOC_TYPE) needs read:wearables instead. Every wearables importer stores its file with it, and
-- the documents stored before this file are moved to it here: a Gadgetbridge import marks its document
-- with flags.gadgetbridge (or stored it as application/vnd.sqlite3), and a FIT document is
-- application/vnd.ant.fit. The CHECK keeps every earlier value (ownhealth_core::documents::DOC_TYPES;
-- check_mirror.rs keeps the two equal). Running it twice changes nothing more.
alter table document drop constraint if exists document_doc_type_check;
alter table document add constraint document_doc_type_check check (doc_type in ('lab_report',
  'imaging_report', 'discharge', 'prescription', 'genomics', 'ecg', 'other', 'wearables'));

update document set doc_type = 'wearables'
 where doc_type is distinct from 'wearables'
   and (flags ? 'gadgetbridge' or mime in ('application/vnd.sqlite3', 'application/vnd.ant.fit'));
