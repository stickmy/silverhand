//! Shared HTTP error responses. Each function is one wire shape; routes pick the shape
//! their contract publishes rather than building bodies inline.

use std::error::Error;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// `500` with the plain-text body `Internal Server Error`; nothing about the cause leaks.
pub(crate) fn plain_500() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

/// Logs the cause chain under `log`, then answers [`plain_500`].
pub(crate) fn logged_500<E: Error + 'static>(cause: E, log: &'static str) -> Response {
    tracing::error!(
        error_chain = super::safe_log_field::safe_display(&(super::error::describe(&cause))),
        "{log}"
    );
    plain_500()
}

/// `{ "error": message }`.
pub(crate) fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// `{ "error": message, "code": code }`.
pub(crate) fn coded_error(status: StatusCode, message: impl Into<String>, code: &str) -> Response {
    (
        status,
        Json(json!({ "error": message.into(), "code": code })),
    )
        .into_response()
}
