//! The built SvelteKit app (`app._mount_ui`). Unknown non-API paths fall back to index.html.
//! The UI is embedded from `server/static` at build time; OWNHEALTH_STATIC_DIR serves a directory
//! instead (as the Python backend did).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use ownhealth_api::ApiError;

#[derive(rust_embed::RustEmbed)]
#[folder = "../../static"]
#[allow_missing = true]
struct Embedded;

#[derive(Debug, Clone)]
pub enum Ui {
    Embedded,
    Dir(PathBuf),
}

impl Ui {
    pub fn from_settings(static_dir: Option<&Path>) -> Ui {
        match static_dir {
            Some(d) => Ui::Dir(d.to_path_buf()),
            None => Ui::Embedded,
        }
    }

    fn read(&self, path: &str) -> Option<Cow<'static, [u8]>> {
        match self {
            Ui::Embedded => Embedded::get(path).map(|f| f.data),
            Ui::Dir(root) => {
                let root = root.canonicalize().ok()?;
                let candidate = root.join(path).canonicalize().ok()?;
                if !candidate.starts_with(&root) || !candidate.is_file() {
                    return None;
                }
                std::fs::read(candidate).ok().map(Cow::Owned)
            }
        }
    }

    pub async fn serve(&self, uri: &Uri) -> Response {
        let path = uri.path().trim_start_matches('/');
        if path == "api" || path == "mcp" || path.starts_with("api/") || path.starts_with("mcp/") {
            let mut r = ApiError::not_found().into_response();
            r.headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            return r;
        }
        if !path.is_empty()
            && let Some(data) = self.read(path)
        {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return file(data, mime.as_ref(), cache_for(path));
        }
        if is_asset(path) {
            return ApiError::not_found().into_response();
        }
        match self.read("index.html") {
            Some(data) => file(data, "text/html; charset=utf-8", Some("no-cache")),
            None => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ui_not_built",
                "The UI is not built. Run `pnpm build` in ui/.",
            )
            .into_response(),
        }
    }
}

/// Hashed build output never changes under its name, so a browser keeps it for a year. The service
/// worker and the version file must be checked on every load, or a new release never reaches it.
fn cache_for(path: &str) -> Option<&'static str> {
    if path.starts_with("_app/immutable/") {
        Some("public, max-age=31536000, immutable")
    } else if path == "service-worker.js" || path == "_app/version.json" {
        Some("no-cache")
    } else {
        None
    }
}

/// A missing build file (a stale hashed chunk, a typo'd icon) answers 404, never the app's HTML:
/// a script tag that gets index.html back fails with a confusing MIME error instead of a clear miss.
/// Page routes have no extension in their last segment, so they still fall back to the app.
fn is_asset(path: &str) -> bool {
    path.starts_with("_app/") || path.rsplit('/').next().is_some_and(|last| last.contains('.'))
}

/// Every UI response refuses to be framed, so no other site (a sibling subdomain included, which the
/// SameSite=Lax session cookie does not stop) can overlay the OAuth consent screen or any other page
/// and trick a click. `X-Frame-Options` is for browsers without CSP level 2.
fn file(data: Cow<'static, [u8]>, content_type: &str, cache: Option<&'static str>) -> Response {
    let mut r = Response::new(Body::from(data.into_owned()));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        r.headers_mut().insert(header::CONTENT_TYPE, v);
    }
    let h = r.headers_mut();
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(c) = cache {
        r.headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static(c));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_files_cache_forever_and_the_worker_never() {
        assert_eq!(
            cache_for("_app/immutable/chunks/a1b2.js"),
            Some("public, max-age=31536000, immutable")
        );
        assert_eq!(cache_for("service-worker.js"), Some("no-cache"));
        assert_eq!(cache_for("_app/version.json"), Some("no-cache"));
        assert_eq!(cache_for("favicon.png"), None);
    }

    #[test]
    fn missing_files_are_assets_and_pages_are_not() {
        assert!(is_asset("_app/immutable/chunks/gone.js"));
        assert!(is_asset("_app/whatever"));
        assert!(is_asset("icons/missing.png"));
        assert!(!is_asset("timeline"));
        assert!(!is_asset("biomarkers/ldl"));
        assert!(!is_asset(""));
    }
}
