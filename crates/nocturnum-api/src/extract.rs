//! Request extractors that answer in the API's error shape and say where a body went wrong.

use axum::body::Bytes;
use axum::extract::{FromRequest, FromRequestParts, Query, Request};
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use serde::de::DeserializeOwned;

use crate::error::ApiError;

/// A JSON request body, refused unless it is sent as `application/json`.
///
/// Requiring the content type means a browser cannot send one from another site without
/// asking first, which a plain form post could, and the server never answers that question
/// with a yes. An empty body reads as `{}`, so endpoints whose fields are all optional take
/// none.
pub struct Body<T>(pub T);

/// Bodies larger than this are parsed on a blocking thread, so a large upsert never holds up
/// the async workers that answer health checks.
const PARSE_INLINE_BYTES: usize = 64 * 1024;

impl<S: Send + Sync, T: DeserializeOwned + Send + 'static> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, ApiError> {
        let json = req
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(is_json);
        let bytes = Bytes::from_request(req, state).await.map_err(|e| {
            if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                ApiError::new(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "too_large",
                    "the request body is over the size limit",
                )
            } else {
                ApiError::invalid(e.body_text())
            }
        })?;
        if bytes.is_empty() {
            return parse(b"{}").map(Body);
        }
        if !json {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "unsupported_media_type",
                "send the body as JSON with Content-Type: application/json",
            ));
        }
        if bytes.len() <= PARSE_INLINE_BYTES {
            return parse(&bytes).map(Body);
        }
        tokio::task::spawn_blocking(move || parse(&bytes))
            .await
            .map_err(|e| ApiError::internal(format!("parsing a body: {e}")))?
            .map(Body)
    }
}

fn is_json(content_type: &str) -> bool {
    let essence = content_type.split(';').next().unwrap_or("").trim();
    essence.eq_ignore_ascii_case("application/json")
}

/// Parses JSON, pointing errors at the field that failed, such as `points[2].id`.
pub fn parse<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, ApiError> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let value = serde_path_to_error::deserialize(&mut de).map_err(|e| {
        let at = e.path().to_string();
        let error = ApiError::invalid(e.into_inner().to_string());
        if at == "." { error } else { error.at(at) }
    })?;
    de.end()
        .map_err(|e| ApiError::invalid(format!("trailing characters after the body: {e}")))?;
    Ok(value)
}

/// Query parameters, refused when one is unknown or malformed.
pub struct Params<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, ApiError> {
        Query::<T>::try_from_uri(&parts.uri)
            .map(|q| Params(q.0))
            .map_err(|e| ApiError::invalid(e.body_text()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Outer {
        points: Vec<Inner>,
    }

    #[derive(Debug, serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    #[allow(dead_code)]
    struct Inner {
        id: u64,
    }

    #[test]
    fn errors_point_at_the_field() {
        let e = parse::<Outer>(br#"{"points": [{"id": 1}, {"id": "x"}]}"#).unwrap_err();
        assert_eq!(e.at.as_deref(), Some("points[1].id"));
        let e = parse::<Outer>(br#"{"points": [], "extra": 1}"#).unwrap_err();
        assert!(e.message.contains("extra"), "{}", e.message);
        let e = parse::<Outer>(b"{}").unwrap_err();
        assert_eq!(e.at, None);
        assert!(parse::<Outer>(br#"{"points": []} x"#).is_err());
    }

    #[test]
    fn json_content_types() {
        assert!(is_json("application/json"));
        assert!(is_json("Application/JSON; charset=utf-8"));
        assert!(!is_json("text/plain"));
        assert!(!is_json("application/jsonx"));
    }
}
