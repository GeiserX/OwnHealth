//! v1.0 imaging: DICOM studies from hospital CDs and zips, into Orthanc and onto the record (slice 41).
//!
//! `POST /api/imaging/import` (ownhealth-api `routes/imaging.rs`) takes a zip, or the files of a CD
//! folder, which [`upload`] zips. The zip is stored as one blob of the profile before anything reads
//! it, recorded as an `imaging_import`, and the `import.dicom` job ([`import`]) sends its studies to
//! Orthanc and records each one. The viewer reads them through the app's proxy
//! (`ownhealth-api/src/routes/imaging_proxy.rs`).
//!
//! Orthanc follows the record (slice 43): [`sweep`] deletes what no record names (a deleted profile's
//! studies, even when Orthanc was down at the delete) and queues [`rebuild`], which fills Orthanc again
//! from the stored zips after a restore. [`remove`] takes back an upload that went to the wrong
//! profile.

pub mod import;
pub mod orthanc;
pub mod part10;
pub mod rebuild;
pub mod remove;
pub mod sweep;
pub mod upload;

/// Registers the imaging jobs (`import.dicom`, and slice 43's `imaging.sweep`, `imaging.rebuild` and
/// `imaging.release_upload`) and the accept handler of `imaging_study` review items.
pub fn register() {
    ownhealth_core::jobs::register_handler(import::JOB, import::job);
    ownhealth_core::jobs::register_handler(sweep::JOB, sweep::job);
    ownhealth_core::jobs::register_handler(rebuild::JOB, rebuild::job);
    ownhealth_core::jobs::register_handler(remove::RELEASE_JOB, remove::release_job);
    ownhealth_core::review::register_accept_handler(import::REVIEW_KIND, |conn, item, user, edits| {
        Box::pin(import::accept(conn, item, user, edits))
    });
}
