//! HTTP-facing error taxonomy for `cmafly-serve`.
//!
//! Public surface is the [`ApiError`] enum; handlers and the
//! [`crate::registry::IndexRegistry`] return `Result<_, ApiError>`. The
//! enum's `IntoResponse` impl is the single point that maps errors to
//! status codes / headers:
//!
//! | Variant              | Status | Notes                                              |
//! |----------------------|--------|----------------------------------------------------|
//! | `BadRequest`         | 400    | `:id` failed `^[a-zA-Z0-9_-]{1,64}$` validation.   |
//! | `NotFound`           | 404    | `.idx` missing, or `:idx` past `segment_count()`.  |
//! | `ServiceUnavailable` | 503    | Admission permit timeout. Sets `Retry-After: 1`.   |
//! | `Internal`           | 500    | Source MP4 missing / `.idx` malformed / I/O error. |
//!
//! Bodies are intentionally generic strings — leaking `idx_path`,
//! file-system layout, or `PackagerError` detail to clients is an audit
//! risk. Internal detail is captured in [`ApiError::Internal`]'s carried
//! string and printed to stderr for the server log only.

use std::fmt;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    /// `:id` outside `^[a-zA-Z0-9_-]{1,64}$`. Static reason aids logging.
    BadRequest(&'static str),
    /// `.idx` not present, or `:idx` query past `segment_count()`.
    NotFound,
    /// Segment-assembly admission permit timed out.
    ServiceUnavailable,
    /// Server-side condition the client cannot fix. Carries a log string.
    Internal(String),
}

impl ApiError {
    /// Helper: wrap an `io::Error` against a path-like context for logs.
    pub fn internal_io(action: &str, path: &std::path::Path, err: std::io::Error) -> Self {
        Self::Internal(format!("{action} {}: {err}", path.display()))
    }

    /// Helper: wrap a [`cmafly::PackagerError`] from
    /// `IndexView::open` or `write_media_segment`.
    pub fn internal_packager(action: &str, id: &str, err: cmafly::PackagerError) -> Self {
        Self::Internal(format!("{action} for `{id}`: {err}"))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadRequest(reason) => write!(f, "bad request: {reason}"),
            Self::NotFound => f.write_str("not found"),
            Self::ServiceUnavailable => f.write_str("service unavailable"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        match self {
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad request").into_response(),
            Self::NotFound => (StatusCode::NOT_FOUND, "not found").into_response(),
            Self::ServiceUnavailable => (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                "service unavailable",
            )
                .into_response(),
            Self::Internal(msg) => {
                // Log the internal detail to stderr so operators can
                // diagnose 500s (typical causes: `.idx` magic mismatch,
                // source-mp4 inconsistency, mmap / I/O failures). The
                // response body stays generic so the wire never carries
                // path or layout information.
                eprintln!("error: {msg}");
                (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    #[tokio::test]
    async fn bad_request_maps_to_400() {
        let resp = ApiError::BadRequest("bad chars").into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn not_found_maps_to_404() {
        let resp = ApiError::NotFound.into_response();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn internal_maps_to_500_without_leaking_detail() {
        let resp = ApiError::Internal("secret path /etc/foo".into()).into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = to_bytes(resp.into_body(), 1024).await.expect("body");
        let body = std::str::from_utf8(&bytes).expect("utf8");
        assert!(!body.contains("/etc/foo"), "internal detail must not leak");
    }

    #[tokio::test]
    async fn service_unavailable_sets_retry_after() {
        let resp = ApiError::ServiceUnavailable.into_response();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let retry_after = resp
            .headers()
            .get(header::RETRY_AFTER)
            .expect("Retry-After header set");
        assert_eq!(retry_after.to_str().unwrap(), "1");
    }
}
