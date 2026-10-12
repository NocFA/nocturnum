//! The one error shape every endpoint returns, see `docs/api/v1.md`.

use std::fmt;

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use nocturnum_query::{Collection, QueryError};
use serde_json::json;

/// An error response: a status, a stable code for programs, a message for people, and where
/// in the request body the problem is when it is in the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub at: Option<String>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            code,
            message: message.into(),
            at: None,
        }
    }

    /// Points the error at a place in the request body, such as `points[3].vector`.
    pub fn at(mut self, at: impl Into<String>) -> ApiError {
        self.at = Some(at.into());
        self
    }

    /// Points the error inside `prefix`, such as `prefetch[1]`, keeping where inside it.
    pub fn within(mut self, prefix: &str) -> ApiError {
        self.at = Some(match self.at {
            Some(at) => format!("{prefix}.{at}"),
            None => prefix.to_owned(),
        });
        self
    }

    pub fn invalid(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn unavailable(message: impl Into<String>) -> ApiError {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    /// Logs the details and answers with a generic message, since internal errors can name
    /// paths on the server.
    pub fn internal(error: impl fmt::Display) -> ApiError {
        tracing::error!("internal error: {error}");
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal error, the server log has the details",
        )
    }

    /// Maps an error from an operation on `collection`, which can tell a storage failure that
    /// has stopped the collection apart from one that has not.
    pub fn from_query(collection: &Collection, error: QueryError) -> ApiError {
        match error {
            QueryError::Invalid(message) => ApiError::invalid(message),
            QueryError::Field { at, message } => ApiError::invalid(message).at(at),
            QueryError::Prefetch(i, e) => {
                ApiError::from_query(collection, *e).within(&format!("prefetch[{i}]"))
            }
            e @ (QueryError::NoTextIndex(_) | QueryError::TooManyWords) => {
                ApiError::invalid(e.to_string())
            }
            QueryError::Input(e) => e.into(),
            QueryError::Storage(e) => ApiError::from_storage(collection.failure(), e),
            QueryError::TimedOut(message) => ApiError::unavailable(message),
        }
    }

    /// Maps a storage error, given what [`Collection::failure`] said.
    pub fn from_storage(failure: Option<&str>, error: nocturnum_storage::Error) -> ApiError {
        use nocturnum_storage::Error as E;
        match error {
            E::Poisoned => ApiError::unavailable(E::Poisoned.to_string()),
            E::RecordTooLarge { size, max } => ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                format!("the operation takes {size} bytes in the log, over the {max} byte limit"),
            ),
            e => match failure {
                Some(why) => {
                    tracing::error!("storage error: {e}");
                    ApiError::unavailable(format!("{why}, restart the server to recover"))
                }
                None => ApiError::internal(e),
            },
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.status.as_u16(),
            self.code,
            self.message
        )
    }
}

impl From<nocturnum_core::Error> for ApiError {
    fn from(e: nocturnum_core::Error) -> ApiError {
        use nocturnum_core::Error as E;
        let message = e.to_string();
        match e {
            E::DimensionMismatch { .. } | E::NonFiniteVector { .. } | E::ZeroVector => {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_vector", message)
            }
            E::InvalidFilter { at, reason } => {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_filter", reason).at(at)
            }
            E::PayloadTooDeep { .. } => {
                ApiError::new(StatusCode::BAD_REQUEST, "invalid_payload", message)
            }
            E::InvalidPath { .. } | E::InvalidConfig(_) => ApiError::invalid(message),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = json!({"code": self.code, "message": self.message});
        if let Some(at) = self.at {
            body["at"] = at.into();
        }
        let mut response = (self.status, Json(json!({ "error": body }))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
