//! The calls the importer makes to Orthanc's REST API, as the app's own account.

use std::time::Duration;

use ownhealth_core::orthanc::OrthancConfig;
use reqwest::StatusCode;
use serde_json::Value;

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

    /// The Orthanc ids of every study under the StudyInstanceUID (`POST /tools/lookup`). Two patients'
    /// studies can share one UID, so this can be more than one.
    pub async fn studies_under(&self, study_instance_uid: &str) -> Result<Vec<String>, OrthancError> {
        let res = self
            .send(
                self.http
                    .post(self.url("/tools/lookup"))
                    .body(study_instance_uid.to_string()),
            )
            .await?;
        let found = Orthanc::json(res, "a UID lookup").await?;
        Ok(found
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|r| r["Type"] == "Study")
                    .filter_map(|r| r["ID"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
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

    /// The series of one study and how many instances Orthanc holds in each (`GET
    /// /studies/{id}/series`, which lists each series with its instance ids). Empty when Orthanc does
    /// not hold the study.
    pub async fn series_of(&self, id: &str) -> Result<Vec<SeriesInfo>, OrthancError> {
        if !is_orthanc_id(id) {
            return Err(OrthancError("Orthanc named a study with an id that is not one".into()));
        }
        let res = self
            .send(self.http.get(self.url(&format!("/studies/{id}/series"))))
            .await?;
        if res.status() == StatusCode::NOT_FOUND {
            return Ok(Vec::new());
        }
        let v = Orthanc::json(res, "a study's series").await?;
        let mut out: Vec<SeriesInfo> = v
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|s| SeriesInfo {
                        uid: s["MainDicomTags"]["SeriesInstanceUID"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        modality: s["MainDicomTags"]["Modality"]
                            .as_str()
                            .unwrap_or_default()
                            .trim()
                            .to_string(),
                        instances: s["Instances"].as_array().map_or(0, Vec::len),
                    })
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        Ok(out)
    }

    /// Sends one file (`POST /instances`).
    pub async fn store(&self, file: Vec<u8>) -> Result<Stored, OrthancError> {
        let res = self
            .send(
                self.http
                    .post(self.url("/instances"))
                    .header(reqwest::header::CONTENT_TYPE, "application/dicom")
                    .body(file),
            )
            .await?;
        if res.status() == StatusCode::BAD_REQUEST {
            return Ok(Stored::Refused);
        }
        let v = Orthanc::json(res, "a file").await?;
        let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let (id, study, status) = (text("ID"), text("ParentStudy"), text("Status"));
        if !is_orthanc_id(&id) || !is_orthanc_id(&study) {
            return Err(OrthancError(
                "Orthanc's answer to a file names no instance and study".into(),
            ));
        }
        match status.as_str() {
            "Success" => Ok(Stored::Kept { id, study, new: true }),
            "AlreadyStored" => Ok(Stored::Kept { id, study, new: false }),
            _ => Ok(Stored::Refused),
        }
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

/// An Orthanc id: five groups of eight hex digits.
pub fn is_orthanc_id(s: &str) -> bool {
    s.len() == 44
        && s.split('-').count() == 5
        && s.split('-')
            .all(|g| g.len() == 8 && g.bytes().all(|b| b.is_ascii_hexdigit()))
}
