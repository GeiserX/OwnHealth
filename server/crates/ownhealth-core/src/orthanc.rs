//! Orthanc (v1.0, compose profile `imaging`): where it is, and the one lock every writer takes.
//!
//! Orthanc knows nothing of profiles; the app is its only client. The viewer proxy
//! (`ownhealth-api/src/routes/imaging_proxy.rs`) reads from it, and every writer (the DICOM import of
//! `ownhealth-imaging`, and later the rebuild from the stored zips) sends files to it under
//! [`lock_study`].

use std::fmt;

use sqlx::PgConnection;

use crate::config::Env;
use crate::error::{Error, Result};

/// Where Orthanc is. `OWNHEALTH_ORTHANC_URL` (Orthanc's root, `http://orthanc:8042` in compose) turns
/// imaging on; the password comes from `OWNHEALTH_ORTHANC_PASSWORD_FILE` (a docker secret) or
/// `OWNHEALTH_ORTHANC_PASSWORD`, the user from `OWNHEALTH_ORTHANC_USER` (default `ownhealth`).
#[derive(Clone, PartialEq, Eq)]
pub struct OrthancConfig {
    pub url: String,
    pub user: String,
    pub password: String,
}

impl fmt::Debug for OrthancConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrthancConfig")
            .field("url", &self.url)
            .field("user", &self.user)
            .field("password", &"***")
            .finish()
    }
}

impl OrthancConfig {
    /// None when `OWNHEALTH_ORTHANC_URL` is unset or empty (imaging off). A URL without a password,
    /// or one that is not http(s), is an error, so a half-configured install does not start.
    pub fn from_env(env: &dyn Env) -> Result<Option<OrthancConfig>> {
        let var = |name: &str| env.var(name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let Some(url) = var("OWNHEALTH_ORTHANC_URL") else {
            return Ok(None);
        };
        let url = url.trim_end_matches('/').to_string();
        let scheme_ok = url.starts_with("http://") || url.starts_with("https://");
        if !scheme_ok || url.contains(['?', '#', '@']) {
            return Err(Error::Invalid(
                "OWNHEALTH_ORTHANC_URL must be Orthanc's http(s) root without credentials, a query, or a fragment. For example, use http://orthanc:8042."
                    .into(),
            ));
        }
        let password = match var("OWNHEALTH_ORTHANC_PASSWORD_FILE") {
            Some(path) => std::fs::read_to_string(&path)
                .map_err(|e| Error::Invalid(format!("OWNHEALTH_ORTHANC_PASSWORD_FILE={path}: {e}")))?
                .trim()
                .to_string(),
            None => var("OWNHEALTH_ORTHANC_PASSWORD").unwrap_or_default(),
        };
        if password.is_empty() {
            return Err(Error::Invalid(
                "OWNHEALTH_ORTHANC_URL is set but Orthanc's password is not: set OWNHEALTH_ORTHANC_PASSWORD_FILE \
                 or OWNHEALTH_ORTHANC_PASSWORD."
                    .into(),
            ));
        }
        let user = var("OWNHEALTH_ORTHANC_USER").unwrap_or_else(|| "ownhealth".into());
        Ok(Some(OrthancConfig { url, user, password }))
    }
}

/// The one lock on a StudyInstanceUID that every Orthanc writer holds, from its check of what Orthanc
/// and the record already hold under the UID until its record of the study commits. Orthanc keys a
/// study by PatientID and UID, so two writers that each checked first could still put two patients'
/// studies under one UID; under this lock the second one sees the first one's study and sends
/// nothing. It is a transaction lock: the caller's transaction ends it.
pub async fn lock_study(conn: &mut PgConnection, study_instance_uid: &str) -> Result<()> {
    sqlx::query("select pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("ownhealth.orthanc.study:{study_instance_uid}"))
        .execute(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[test]
    fn the_error_for_a_url_with_credentials_omits_the_url() {
        let sentinel = crate::ids::token_hex(16);
        let env = HashMap::from([
            (
                "OWNHEALTH_ORTHANC_URL".to_string(),
                format!("http://ownhealth:{sentinel}@orthanc:8042"),
            ),
            ("OWNHEALTH_ORTHANC_PASSWORD".to_string(), crate::ids::token_hex(16)),
        ]);

        let Err(Error::Invalid(message)) = OrthancConfig::from_env(&env) else {
            panic!("a URL with credentials must be refused");
        };
        assert!(message.contains("OWNHEALTH_ORTHANC_URL"));
        assert!(!message.contains(&sentinel), "the error contains the URL credential");
    }
}
