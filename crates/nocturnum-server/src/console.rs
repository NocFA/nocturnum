//! The web console, a static SvelteKit build from `console/build` embedded in the binary.
//!
//! Release builds carry the files inside the binary, while debug builds read them from
//! `console/build` as they are asked for, so `npm run build` shows up without recompiling.
//! A path that names no file gets `index.html`, and the app routes itself in the browser.
//! When the console was never built the server says how to build it instead.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use nocturnum_api::Console;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "../../console/build"]
#[allow_missing = true]
struct Assets;

const NOT_BUILT: &str = "The web console was not built into this binary.\n\
Build it with `npm --prefix console ci && npm --prefix console run build`, then build nocturnum again.\n\
The API is unaffected, see docs/api/v1.md.\n";

/// Headers every console response carries: no framing by other sites, no MIME sniffing and
/// no referrer leaking a collection name to a link's target.
fn harden(response: &mut Response) {
    let h = response.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("frame-ancestors 'none'; object-src 'none'; base-uri 'self'"),
    );
    h.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    h.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
}

/// Whether `name` resolves inside the build folder. Debug builds read assets from disk, and
/// rust-embed follows a symlink out of the folder, so the resolved path is checked too.
/// Release builds embed at compile time, where `build.rs` refuses any symlink.
#[cfg(debug_assertions)]
fn contained(name: &str) -> bool {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../console/build");
    if std::fs::symlink_metadata(&root).is_ok_and(|m| m.file_type().is_symlink()) {
        return false;
    }
    match (root.canonicalize(), root.join(name).canonicalize()) {
        (Ok(root), Ok(file)) => file.starts_with(root),
        _ => false,
    }
}

#[cfg(not(debug_assertions))]
fn contained(_: &str) -> bool {
    true
}

fn file(path: &str, method: &Method, headers: &HeaderMap) -> Option<Response> {
    if !contained(path) {
        return None;
    }
    let asset = Assets::get(path)?;
    let etag = format!("\"{}\"", hex(&asset.metadata.sha256_hash()[..16]));
    // Files under _app/immutable carry a content hash in their names, so they never change.
    let cache = if path.starts_with("_app/immutable/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let fresh = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    let mut response = if fresh {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let len = asset.data.len();
        let body = if method == Method::HEAD {
            Body::empty()
        } else {
            Body::from(asset.data.into_owned())
        };
        let mut r = Response::new(body);
        r.headers_mut()
            .insert(header::CONTENT_LENGTH, HeaderValue::from(len));
        if let Ok(v) = HeaderValue::from_str(asset.metadata.mimetype()) {
            r.headers_mut().insert(header::CONTENT_TYPE, v);
        }
        r
    };
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    if let Ok(v) = HeaderValue::from_str(&etag) {
        h.insert(header::ETAG, v);
    }
    Some(response)
}

/// A request path as an asset name, or `None` when it could step outside the build folder.
///
/// Debug builds read assets from disk, so `..`, `.`, empty segments, backslashes and NUL
/// bytes are refused before rust-embed sees them. Percent escapes are left as they are, so
/// `%2e%2e` names a file literally called that, which does not exist.
fn asset_name(path: &str) -> Option<&str> {
    let name = path.strip_prefix('/')?;
    if name.is_empty() {
        return Some(name);
    }
    let safe = name
        .split('/')
        .all(|seg| !seg.is_empty() && seg != "." && seg != ".." && !seg.contains(['\\', '\0']));
    safe.then_some(name)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn serve(method: &Method, uri: &Uri, headers: &HeaderMap) -> Response {
    let mut response = if method != Method::GET && method != Method::HEAD {
        let mut r = StatusCode::METHOD_NOT_ALLOWED.into_response();
        r.headers_mut()
            .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
        r
    } else {
        let Some(path) = asset_name(uri.path()) else {
            let mut r = StatusCode::NOT_FOUND.into_response();
            harden(&mut r);
            return r;
        };
        let last = path.rsplit('/').next().unwrap_or("");
        match (!path.is_empty())
            .then(|| file(path, method, headers))
            .flatten()
        {
            Some(r) => r,
            // A missing file with an extension is a real 404, so a broken asset link fails
            // loudly instead of loading the app in its place.
            None if last.contains('.') => StatusCode::NOT_FOUND.into_response(),
            None => match file("index.html", method, headers) {
                Some(r) => r,
                None => (
                    StatusCode::NOT_FOUND,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    NOT_BUILT,
                )
                    .into_response(),
            },
        }
    };
    harden(&mut response);
    response
}

/// The console, for [`nocturnum_api::Api::router_with_console`].
pub(crate) fn console() -> Console {
    Arc::new(serve)
}

/// Whether this binary carries a built console.
pub(crate) fn built() -> bool {
    Assets::get("index.html").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get(path: &str) -> Response {
        serve(&Method::GET, &path.parse().unwrap(), &HeaderMap::new())
    }

    #[test]
    fn app_paths_get_the_app_and_missing_files_get_404() {
        for path in ["/", "/collections/notes/points", "/keys"] {
            let r = get(path);
            if built() {
                assert_eq!(r.status(), StatusCode::OK, "{path}");
                assert_eq!(r.headers()[header::CONTENT_TYPE], "text/html");
                assert_eq!(r.headers()[header::CACHE_CONTROL], "no-cache");
            } else {
                assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
            }
            assert_eq!(r.headers()[header::X_FRAME_OPTIONS], "DENY");
            assert_eq!(r.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        }
        assert_eq!(get("/_app/missing.js").status(), StatusCode::NOT_FOUND);
        assert_eq!(get("/robots.txt.bak").status(), StatusCode::NOT_FOUND);
        let post = serve(&Method::POST, &"/".parse().unwrap(), &HeaderMap::new());
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(post.headers()[header::ALLOW], "GET, HEAD");
    }

    #[test]
    fn paths_cannot_step_outside_the_build() {
        for path in [
            "/../../CLAUDE.md",
            "/../Cargo.toml",
            "/_app/../../../Cargo.toml",
            "/./index.html",
            "//etc/passwd",
            "/a\\..\\b",
            "/_app//x.js",
        ] {
            let r = get(path);
            assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
            assert_eq!(r.headers()[header::X_FRAME_OPTIONS], "DENY");
        }
        assert_eq!(asset_name("/"), Some(""));
        assert_eq!(
            asset_name("/_app/immutable/a.js"),
            Some("_app/immutable/a.js")
        );
        assert_eq!(asset_name("/a/../b"), None);
    }

    #[test]
    fn a_symlink_out_of_the_build_is_not_served() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../console/build");
        if !root.is_dir() {
            return;
        }
        let link = root.join("escape-test.txt");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
            &link,
        )
        .unwrap();
        let r = get("/escape-test.txt");
        std::fs::remove_file(&link).unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn hashed_files_are_cached_for_good_and_revalidate_by_etag() {
        let Some(name) = Assets::iter().find(|f| f.starts_with("_app/immutable/")) else {
            // Nothing to check without a built console.
            return;
        };
        let r = get(&format!("/{name}"));
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(
            r.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let etag = r.headers()[header::ETAG].clone();
        let mut h = HeaderMap::new();
        h.insert(header::IF_NONE_MATCH, etag);
        let again = serve(&Method::GET, &format!("/{name}").parse().unwrap(), &h);
        assert_eq!(again.status(), StatusCode::NOT_MODIFIED);
        let head = serve(
            &Method::HEAD,
            &format!("/{name}").parse().unwrap(),
            &HeaderMap::new(),
        );
        assert_eq!(head.status(), StatusCode::OK);
        assert!(
            head.headers()[header::CONTENT_LENGTH]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                > 0
        );
    }
}
