//! The calls the importer makes to Orthanc's REST API, as the app's own account.

use std::fs::File;
use std::time::Duration;

use ownhealth_core::orthanc::OrthancConfig;
use reqwest::StatusCode;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// What went wrong talking to Orthanc. Every one of them is worth a retry of the job, except a file
/// Orthanc refused ([`Stored::Refused`]), which is not an error.
#[derive(Debug)]
pub struct OrthancError(pub String);

impl std::fmt::Display for OrthancError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for OrthancError {}

/// The answer to one `POST /instances`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stored {
    /// Orthanc holds the instance: its id, the id of the study it filed it under, and whether it was
    /// new (`Success`) or there already (`AlreadyStored`).
    Kept { id: String, study: String, new: bool },
    /// Orthanc did not take the file (it does not parse it as DICOM).
    Refused,
}

/// A study as Orthanc holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudyInfo {
    pub study_instance_uid: String,
    pub patient_id: String,
}

/// One series of a study as Orthanc holds it: what the record counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesInfo {
    pub uid: String,
    pub modality: String,
    pub instances: usize,
}

/// Orthanc's answers are JSON of a few kilobytes; nothing it says to the importer is larger.
const MAX_ANSWER: usize = 4 << 20;

#[derive(Clone)]
pub struct Orthanc {
    config: OrthancConfig,
    http: reqwest::Client,
}

impl Orthanc {
    pub fn new(config: OrthancConfig) -> Result<Orthanc, OrthancError> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .read_timeout(Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| OrthancError(format!("the HTTP client for Orthanc does not start: {e}")))?;
        Ok(Orthanc { config, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.url)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<reqwest::Response, OrthancError> {
        let res = req
            .basic_auth(&self.config.user, Some(&self.config.password))
            .send()
            .await
            .map_err(|e| OrthancError(format!("Orthanc did not answer: {}", e.without_url())))?;
        if matches!(res.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
            return Err(OrthancError(
                "Orthanc refused OwnHealth's account: check its password".into(),
            ));
        }
        Ok(res)
    }

    async fn json(res: reqwest::Response, what: &str) -> Result<Value, OrthancError> {
        let status = res.status();
        if !status.is_success() {
            return Err(OrthancError(format!("Orthanc answered {status} to {what}")));
        }
        let mut res = res;
        let mut body = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| OrthancError(format!("Orthanc's answer to {what} broke off")))?
        {
            if body.len() + chunk.len() > MAX_ANSWER {
                return Err(OrthancError(format!("Orthanc's answer to {what} is too large")));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| OrthancError(format!("Orthanc's answer to {what} is not JSON")))
    }

    /// The Orthanc IDs of every study under the UID. Two patients can share one StudyInstanceUID.
    pub async fn studies_under(&self, study_instance_uid: &str) -> Result<Vec<String>, OrthancError> {
        self.lookup(study_instance_uid, "Study").await
    }

    /// The Orthanc IDs that share a SOPInstanceUID, across all studies.
    pub(crate) async fn instances_under(&self, sop_uid: &str) -> Result<Vec<String>, OrthancError> {
        self.lookup(sop_uid, "Instance").await
    }

    /// Find a SOP by membership in the recorded study when the upload's PatientID is unreadable.
    pub(crate) async fn instance_in_study(&self, study: &str, sop_uid: &str) -> Result<Option<String>, OrthancError> {
        let res = self
            .send(self.http.post(self.url("/tools/find")).json(&json!({
                "Level": "Instance",
                "ParentStudy": study,
                "Query": {"SOPInstanceUID": sop_uid},
                "Limit": 2
            })))
            .await?;
        let found = Self::json(res, "a SOP lookup in a study").await?;
        let invalid = || OrthancError("Orthanc returned an invalid SOP lookup in a study".into());
        match found.as_array().ok_or_else(invalid)?.as_slice() {
            [] => Ok(None),
            [id] => id
                .as_str()
                .filter(|id| is_orthanc_id(id))
                .map(String::from)
                .map(Some)
                .ok_or_else(invalid),
            _ => Err(invalid()),
        }
    }

    async fn lookup(&self, uid: &str, kind: &str) -> Result<Vec<String>, OrthancError> {
        let res = self
            .send(self.http.post(self.url("/tools/lookup")).body(uid.to_string()))
            .await?;
        let found = Self::json(res, "a UID lookup").await?;
        let invalid = || OrthancError("Orthanc returned an invalid UID lookup".into());
        let rows = found.as_array().ok_or_else(invalid)?;
        let mut ids = Vec::new();
        for row in rows {
            match row["Type"].as_str() {
                Some(t) if t == kind => {
                    let id = row["ID"].as_str().filter(|id| is_orthanc_id(id)).ok_or_else(invalid)?;
                    ids.push(id.to_string());
                }
                Some("Patient" | "Study" | "Series" | "Instance") => {}
                _ => return Err(invalid()),
            }
        }
        Ok(ids)
    }

    /// A lost response may hide an ID computed from tags Orthanc decoded differently. Only the
    /// exact bytes of the attempted upload can establish ownership of such an instance.
    pub(crate) async fn instance_matches(&self, id: &str, sha256: &[u8], size: u64) -> Result<bool, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("not an Orthanc id".into()));
        }
        let mut res = self
            .send(self.http.get(self.url(&format!("/instances/{id}/file"))))
            .await?;
        let status = res.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(false);
        }
        if !status.is_success() {
            return Err(OrthancError(format!("Orthanc answered {status} to an instance read")));
        }
        let mut n = 0;
        let mut digest = Sha256::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| OrthancError("Orthanc's instance read broke off".into()))?
        {
            n += chunk.len() as u64;
            if n > size {
                return Ok(false);
            }
            digest.update(&chunk);
        }
        Ok(n == size && &digest.finalize()[..] == sha256)
    }

    /// Download the retained file to an unlinked spool while bounding its size.
    pub(crate) async fn download_instance(&self, id: &str, file: File, limit: u64) -> Result<File, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("not an Orthanc id".into()));
        }
        let mut res = self
            .send(self.http.get(self.url(&format!("/instances/{id}/file"))))
            .await?;
        if !res.status().is_success() {
            return Err(OrthancError(format!(
                "Orthanc answered {} to an instance read",
                res.status()
            )));
        }
        let mut out = tokio::fs::File::from_std(file);
        let mut size = 0;
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| OrthancError("Orthanc's instance read broke off".into()))?
        {
            size += chunk.len() as u64;
            if size > limit {
                return Err(OrthancError("Orthanc's retained report instance is too large".into()));
            }
            out.write_all(&chunk)
                .await
                .map_err(|e| OrthancError(format!("staging an Orthanc report: {e}")))?;
        }
        out.flush()
            .await
            .map_err(|e| OrthancError(format!("staging an Orthanc report: {e}")))?;
        Ok(out.into_std().await)
    }

    /// One study's UID and patient as Orthanc read them (`GET /studies/{id}`).
    pub async fn study(&self, id: &str) -> Result<StudyInfo, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("Orthanc named a study with an id that is not one".into()));
        }
        let res = self.send(self.http.get(self.url(&format!("/studies/{id}")))).await?;
        let v = Orthanc::json(res, "a study read").await?;
        let tag = |group: &str, name: &str| v[group][name].as_str().unwrap_or_default().trim().to_string();
        Ok(StudyInfo {
            study_instance_uid: tag("MainDicomTags", "StudyInstanceUID"),
            patient_id: tag("PatientMainDicomTags", "PatientID"),
        })
    }

    /// The series of one study and their actual instance counts. Ask Orthanc for the computed
    /// counts, without requesting child IDs. A page is bounded even when a study has many series.
    pub async fn series_of(&self, id: &str) -> Result<Vec<SeriesInfo>, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("Orthanc named a study with an id that is not one".into()));
        }
        const PAGE_SIZE: usize = 128;
        let mut out = Vec::new();
        loop {
            let res = self
                .send(self.http.post(self.url("/tools/find")).json(&json!({
                    "Level": "Series",
                    "Query": {},
                    "ParentStudy": id,
                    "ResponseContent": ["MainDicomTags", "RequestedTags"],
                    "RequestedTags": ["NumberOfSeriesRelatedInstances"],
                    "OrderBy": [{"Type": "DicomTag", "Key": "SeriesInstanceUID", "Direction": "ASC"}],
                    "Limit": PAGE_SIZE,
                    "Since": out.len(),
                })))
                .await?;
            let v = Orthanc::json(res, "a study's series counts").await?;
            let page = v
                .as_array()
                .filter(|p| p.len() <= PAGE_SIZE)
                .ok_or_else(|| OrthancError("Orthanc returned an invalid series page".into()))?;
            for s in page {
                let tags = &s["MainDicomTags"];
                let uid = tags["SeriesInstanceUID"]
                    .as_str()
                    .map(str::trim)
                    .filter(|uid| crate::part10::is_uid(uid))
                    .ok_or_else(|| OrthancError("Orthanc did not return a valid series UID".into()))?;
                if out.last().is_some_and(|prev: &SeriesInfo| prev.uid.as_str() >= uid) {
                    return Err(OrthancError("Orthanc returned a series page out of order".into()));
                }
                let instances = s["RequestedTags"]["NumberOfSeriesRelatedInstances"]
                    .as_str()
                    .and_then(|n| n.trim().parse().ok())
                    .ok_or_else(|| OrthancError("Orthanc did not return a series instance count".into()))?;
                let modality = tags["Modality"].as_str().unwrap_or_default().trim().to_string();
                out.push(SeriesInfo {
                    uid: uid.to_string(),
                    modality,
                    instances,
                });
            }
            if page.len() < PAGE_SIZE {
                return Ok(out);
            }
        }
    }

    /// Sends one file already in memory.
    pub async fn store(&self, file: Vec<u8>) -> Result<Stored, OrthancError> {
        let size = file.len() as u64;
        self.store_body(file.into(), size).await
    }

    /// Stream a staged file with a known Content-Length. Each read is at most one chunk.
    pub(crate) async fn store_file(&self, file: std::fs::File, size: u64) -> Result<Stored, OrthancError> {
        self.store_body(reqwest::Body::wrap_stream(file_chunks(file)), size)
            .await
    }

    async fn store_body(&self, body: reqwest::Body, size: u64) -> Result<Stored, OrthancError> {
        let res = self
            .send(
                self.http
                    .post(self.url("/instances"))
                    .header(reqwest::header::CONTENT_TYPE, "application/dicom")
                    .header(reqwest::header::CONTENT_LENGTH, size)
                    .body(body),
            )
            .await?;
        if res.status() == StatusCode::BAD_REQUEST {
            return Ok(Stored::Refused);
        }
        let v = Orthanc::json(res, "a file").await?;
        let new = match v["Status"].as_str() {
            Some("Success") => true,
            Some("AlreadyStored") => false,
            Some("FilteredOut") => return Ok(Stored::Refused),
            _ => {
                return Err(OrthancError(
                    "Orthanc's answer to a file has an unknown or missing status".into(),
                ));
            }
        };
        let id = v["ID"].as_str().filter(|id| is_orthanc_id(id));
        let study = v["ParentStudy"].as_str().filter(|id| is_orthanc_id(id));
        let (Some(id), Some(study)) = (id, study) else {
            return Err(OrthancError(
                "Orthanc's answer to a file names no instance and study".into(),
            ));
        };
        Ok(Stored::Kept {
            id: id.into(),
            study: study.into(),
            new,
        })
    }

    /// Every study Orthanc holds, by id (`GET /studies`, a page at a time).
    pub async fn study_ids(&self) -> Result<Vec<String>, OrthancError> {
        const PAGE_SIZE: usize = 500;
        let mut out: Vec<String> = Vec::new();
        loop {
            let res = self
                .send(
                    self.http
                        .get(self.url(&format!("/studies?limit={PAGE_SIZE}&since={}", out.len()))),
                )
                .await?;
            let v = Orthanc::json(res, "the study list").await?;
            let invalid = || OrthancError("Orthanc returned an invalid study list".into());
            let page = v.as_array().filter(|p| p.len() <= PAGE_SIZE).ok_or_else(invalid)?;
            for id in page {
                let id = id.as_str().filter(|id| is_orthanc_id(id)).ok_or_else(invalid)?;
                out.push(id.to_string());
            }
            if page.len() < PAGE_SIZE {
                return Ok(out);
            }
        }
    }

    /// The series of one study: each one's Orthanc id and SeriesInstanceUID (`GET /studies/{id}/series`).
    /// None when Orthanc holds no such study.
    pub async fn series_ids(&self, id: &str) -> Result<Option<Vec<(String, String)>>, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("not an Orthanc id".into()));
        }
        let res = self
            .send(self.http.get(self.url(&format!("/studies/{id}/series"))))
            .await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let v = Orthanc::json(res, "a study's series").await?;
        let invalid = || OrthancError("Orthanc returned an invalid series list".into());
        let mut out = Vec::new();
        for s in v.as_array().ok_or_else(invalid)? {
            let sid = s["ID"].as_str().filter(|id| is_orthanc_id(id)).ok_or_else(invalid)?;
            let uid = s["MainDicomTags"]["SeriesInstanceUID"]
                .as_str()
                .map(str::trim)
                .filter(|uid| crate::part10::is_uid(uid))
                .ok_or_else(invalid)?;
            out.push((sid.to_string(), uid.to_string()));
        }
        Ok(Some(out))
    }

    /// Removes a whole study (`DELETE /studies/{id}`); one that is gone already counts as removed.
    pub async fn delete_study(&self, id: &str) -> Result<(), OrthancError> {
        self.delete_resource("studies", id).await
    }

    /// Removes one series (`DELETE /series/{id}`); one that is gone already counts as removed.
    pub async fn delete_series(&self, id: &str) -> Result<(), OrthancError> {
        self.delete_resource("series", id).await
    }

    async fn delete_resource(&self, level: &str, id: &str) -> Result<(), OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("not an Orthanc id".into()));
        }
        let res = self.send(self.http.delete(self.url(&format!("/{level}/{id}")))).await?;
        if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
            return Err(OrthancError(format!("Orthanc answered {} to a delete", res.status())));
        }
        Ok(())
    }

    /// Takes back an instance this import added (`DELETE /instances/{id}`).
    pub async fn delete_instance(&self, id: &str) -> Result<(), OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("not an Orthanc id".into()));
        }
        let res = self
            .send(self.http.delete(self.url(&format!("/instances/{id}"))))
            .await?;
        if !res.status().is_success() && res.status() != StatusCode::NOT_FOUND {
            return Err(OrthancError(format!("Orthanc answered {} to a delete", res.status())));
        }
        Ok(())
    }

    /// Reads one series' DICOMweb metadata once, so the viewer's first open of it finds Orthanc's
    /// metadata cache filled (`ORTHANC__DICOM_WEB__ENABLE_METADATA_CACHE`).
    pub async fn warm_series(&self, study_uid: &str, series_uid: &str) -> Result<(), OrthancError> {
        let mut res = self
            .send(
                self.http
                    .get(self.url(&format!("/dicom-web/studies/{study_uid}/series/{series_uid}/metadata")))
                    .header(reqwest::header::ACCEPT, "application/dicom+json"),
            )
            .await?;
        if !res.status().is_success() {
            return Err(OrthancError(format!(
                "Orthanc answered {} to a series metadata read",
                res.status()
            )));
        }
        // the answer itself is not needed: reading it is what fills the cache
        while res
            .chunk()
            .await
            .map_err(|_| OrthancError("Orthanc's series metadata broke off".into()))?
            .is_some()
        {}
        Ok(())
    }
}

/// DicomInstanceHasher in Orthanc hashes the identifiers in order, separated by pipes.
pub(crate) fn resource_id(parts: &[&str]) -> String {
    let digest = sha1::Sha1::digest(parts.join("|").as_bytes());
    digest
        .as_chunks::<4>()
        .0
        .iter()
        .map(hex::encode)
        .collect::<Vec<_>>()
        .join("-")
}

/// An Orthanc id: five groups of eight hex digits.
pub fn is_orthanc_id(s: &str) -> bool {
    s.len() == 44
        && s.split('-').count() == 5
        && s.split('-')
            .all(|g| g.len() == 8 && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub(crate) const FILE_CHUNK: usize = 64 << 10;

fn file_chunks(file: std::fs::File) -> tokio_util::io::ReaderStream<tokio::fs::File> {
    let mut file = tokio::fs::File::from_std(file);
    file.set_max_buf_size(FILE_CHUNK);
    tokio_util::io::ReaderStream::with_capacity(file, FILE_CHUNK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[tokio::test]
    async fn a_large_instance_is_read_one_chunk_at_a_time() -> std::io::Result<()> {
        let file = tempfile::tempfile()?;
        let size = 33 * FILE_CHUNK as u64 + 19;
        file.set_len(size)?;
        let mut chunks = file_chunks(file);
        let mut read = 0;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            assert!(chunk.len() <= FILE_CHUNK);
            read += chunk.len() as u64;
        }
        assert_eq!(read, size);
        Ok(())
    }
}
