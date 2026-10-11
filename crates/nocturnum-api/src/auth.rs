//! API keys and the host check, applied before a handler reads the request body.
//!
//! With keys configured, every request but the health checks needs one, sent as
//! `Authorization: Bearer <key>`. The admin key can do everything, and the read-only key can
//! send `GET` requests and the four point reads that use `POST`.
//!
//! Without keys the server only listens on loopback unless told otherwise, and then any web
//! page the owner opens could try to reach it:
//!
//! - through a hostname of its own that resolves to 127.0.0.1, known as DNS rebinding, so
//!   requests whose `Host` is not an IP address, `localhost` or a name in the allowed list
//!   are refused,
//! - by posting to it from another site, which a browser does without asking first for a
//!   form or a body-less request, so a request that can change data and carries an `Origin`
//!   other than the server's own is refused.

use std::net::IpAddr;
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

use crate::Api;
use crate::error::ApiError;

/// The configured keys, kept as hashes so comparing them takes the same time whatever the
/// guess.
pub(crate) struct Keys {
    admin: Option<[u8; 32]>,
    read_only: Option<[u8; 32]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Admin,
    ReadOnly,
}

fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// Compares every byte whatever the first difference, so timing says nothing about how much
/// of a guess was right.
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

impl Keys {
    pub(crate) fn new(admin: Option<&str>, read_only: Option<&str>) -> Keys {
        Keys {
            admin: admin.map(digest),
            read_only: read_only.map(digest),
        }
    }

    pub(crate) fn anonymous(&self) -> bool {
        self.admin.is_none() && self.read_only.is_none()
    }

    fn check(&self, key: &str) -> Option<Access> {
        let key = digest(key);
        // Both are compared, so the time taken does not say which key matched.
        let admin = self.admin.is_some_and(|a| same(&a, &key));
        let read_only = self.read_only.is_some_and(|r| same(&r, &key));
        if admin {
            Some(Access::Admin)
        } else if read_only {
            Some(Access::ReadOnly)
        } else {
            None
        }
    }
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, key) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| key.trim())
        .filter(|k| !k.is_empty())
}

/// Whether the read-only key may make this request. Paths are matched exactly as the router
/// matches them, so no spelling of a write path can pass for a read.
fn read_only_allows(method: &Method, path: &str) -> bool {
    if method == Method::GET || method == Method::HEAD {
        return true;
    }
    let parts: Vec<&str> = path.split('/').collect();
    method == Method::POST
        && matches!(
            parts[..],
            [
                "",
                "v1",
                "collections",
                _,
                "points",
                "search" | "scroll" | "count" | "get"
            ]
        )
}

/// The host a request was sent to, without its port, from the URI for HTTP/2 and the `Host`
/// header for HTTP/1.
fn request_host(req: &Request) -> Option<String> {
    if let Some(host) = req.uri().host() {
        return Some(
            host.trim_start_matches('[')
                .trim_end_matches(']')
                .to_owned(),
        );
    }
    let value = req.headers().get(header::HOST)?.to_str().ok()?;
    if let Some(rest) = value.strip_prefix('[') {
        return Some(rest.split(']').next().unwrap_or("").to_owned());
    }
    Some(match value.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name.to_owned(),
        _ => value.to_owned(),
    })
}

/// Whether a browser sent the request from a page the server itself served, or the request
/// did not come from a browser at all, which sends no `Origin`.
fn same_origin(req: &Request) -> bool {
    let Some(origin) = req.headers().get(header::ORIGIN) else {
        return true;
    };
    let authority = origin
        .to_str()
        .ok()
        .and_then(|o| o.split_once("://"))
        .map(|(_, a)| a);
    let host = match req.uri().authority() {
        Some(a) => Some(a.as_str()),
        None => req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok()),
    };
    matches!((authority, host), (Some(a), Some(h)) if a.eq_ignore_ascii_case(h))
}

fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let host = host.trim_end_matches('.');
    host.parse::<IpAddr>().is_ok()
        || host.eq_ignore_ascii_case("localhost")
        || allowed.iter().any(|a| a.eq_ignore_ascii_case(host))
}

/// Middleware that checks the key, or the host when no keys are configured.
pub(crate) async fn guard(State(api): State<Arc<Api>>, req: Request, next: Next) -> Response {
    if api.keys.anonymous() {
        if let Some(host) = request_host(&req)
            && !host_allowed(&host, &api.allowed_hosts)
        {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                format!(
                    "requests for host {host:?} are refused while no API keys are set, \
                     add it to server.allowed_hosts"
                ),
            )
            .into_response();
        }
        let reads = [Method::GET, Method::HEAD, Method::OPTIONS];
        if !reads.contains(req.method()) && !same_origin(&req) {
            return ApiError::new(
                StatusCode::FORBIDDEN,
                "forbidden",
                "requests from other sites that change data are refused while no API keys are set",
            )
            .into_response();
        }
        return next.run(req).await;
    }
    let Some(access) = bearer(req.headers()).and_then(|k| api.keys.check(k)) else {
        return ApiError::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "send a valid API key as Authorization: Bearer <key>",
        )
        .into_response();
    };
    if access == Access::ReadOnly && !read_only_allows(req.method(), req.uri().path()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the read-only key cannot make changes",
        )
        .into_response();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_exactly() {
        let keys = Keys::new(Some("example-admin"), Some("example-reader"));
        assert_eq!(keys.check("example-admin"), Some(Access::Admin));
        assert_eq!(keys.check("example-reader"), Some(Access::ReadOnly));
        assert_eq!(keys.check("example-admi"), None);
        assert_eq!(keys.check(""), None);
        assert!(!keys.anonymous());
        assert!(Keys::new(None, None).anonymous());
    }

    #[test]
    fn bearer_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(bearer(&h), Some("abc"));
        h.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(bearer(&h), None);
    }

    #[test]
    fn read_only_paths() {
        let p = "/v1/collections/notes/points/search";
        assert!(read_only_allows(&Method::POST, p));
        assert!(read_only_allows(&Method::GET, "/v1/collections"));
        assert!(!read_only_allows(&Method::PUT, p));
        assert!(!read_only_allows(
            &Method::POST,
            "/v1/collections/notes/points/delete"
        ));
        assert!(!read_only_allows(
            &Method::POST,
            "/v1/collections/notes/points/search/"
        ));
        assert!(!read_only_allows(
            &Method::POST,
            "/v1/collections/notes/points/%73earch"
        ));
        assert!(!read_only_allows(&Method::DELETE, "/v1/collections/notes"));
        assert!(!read_only_allows(
            &Method::POST,
            "/v1/collections/notes/snapshots"
        ));
    }

    #[test]
    fn hosts() {
        let allowed = vec!["my-server".to_owned()];
        for ok in [
            "127.0.0.1",
            "::1",
            "localhost",
            "LOCALHOST",
            "my-server",
            "192.0.2.10",
        ] {
            assert!(host_allowed(ok, &allowed), "{ok}");
        }
        for bad in ["evil.example", "127.0.0.1.nip.io", "localhost.evil", ""] {
            assert!(!host_allowed(bad, &allowed), "{bad}");
        }
    }
}
