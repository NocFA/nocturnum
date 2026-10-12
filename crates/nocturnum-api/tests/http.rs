//! The REST API over real HTTP, against `docs/api/v1.md`.

use std::path::Path;
use std::sync::Arc;

use nocturnum_api::{Api, ApiOptions, Registry, RegistryOptions};
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use tempfile::TempDir;

const ADMIN: &str = "example-admin-key-01";
const READER: &str = "reader-key-0123456789";

struct Server {
    base: String,
    api: Arc<Api>,
    client: reqwest::Client,
    key: Option<&'static str>,
    dir: TempDir,
}

fn registry(data: &Path) -> Registry {
    Registry::open(data, RegistryOptions::default()).unwrap()
}

async fn start_with(options: ApiOptions, load: bool) -> Server {
    let dir = TempDir::new().unwrap();
    let api = Api::new(options);
    if load {
        let data = dir.path().to_owned();
        let r = tokio::task::spawn_blocking(move || registry(&data))
            .await
            .unwrap();
        api.set_registry(r);
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = api.router();
    tokio::spawn(async move { axum::serve(listener, router).await });
    Server {
        base,
        api,
        client: reqwest::Client::new(),
        key: None,
        dir,
    }
}

async fn start() -> Server {
    start_with(ApiOptions::default(), true).await
}

async fn start_with_keys() -> Server {
    let mut s = start_with(
        ApiOptions {
            admin_key: Some(ADMIN.into()),
            read_only_key: Some(READER.into()),
            ..ApiOptions::default()
        },
        true,
    )
    .await;
    s.key = Some(ADMIN);
    s
}

impl Server {
    fn request(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.client.request(method, format!("{}{path}", self.base));
        match self.key {
            Some(k) => r.bearer_auth(k),
            None => r,
        }
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut r = self.request(method, path);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let resp = r.send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, value)
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.call(Method::GET, path, None).await
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::POST, path, Some(body)).await
    }

    async fn put(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(Method::PUT, path, Some(body)).await
    }

    /// Calls and expects 200.
    async fn ok(&self, method: Method, path: &str, body: Option<Value>) -> Value {
        let (status, v) = self.call(method, path, body).await;
        assert_eq!(status, StatusCode::OK, "{path}: {v}");
        v
    }

    async fn count(&self, body: Value) -> Value {
        self.post("/v1/collections/c/points/count", body).await.1
    }

    /// A write to collection `c` that has to succeed, returning its sequence number.
    async fn write(&self, path: &str, body: Value) -> u64 {
        let (status, v) = self
            .post(&format!("/v1/collections/c/points/{path}"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{path}: {v}");
        assert_eq!(v["ack"], "durable");
        v["seq"].as_u64().unwrap()
    }

    async fn create(&self, name: &str, dim: usize, distance: &str) {
        let v = self
            .ok(
                Method::PUT,
                &format!("/v1/collections/{name}"),
                Some(json!({"vectors": {"dim": dim, "distance": distance}})),
            )
            .await;
        assert_eq!(v, json!({"created": true}));
    }
}

fn code(v: &Value) -> &str {
    v["error"]["code"].as_str().unwrap_or_else(|| panic!("{v}"))
}

fn ids(v: &Value) -> Vec<Value> {
    v["points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].clone())
        .collect()
}

#[tokio::test]
async fn health_version_and_loading() {
    let s = start_with(ApiOptions::default(), false).await;
    assert_eq!(
        s.get("/livez").await,
        (StatusCode::OK, json!({"status": "ok"}))
    );
    let (status, v) = s.get("/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(code(&v), "unavailable");
    for path in ["/v1", "/v1/collections"] {
        let (status, v) = s.get(path).await;
        assert_eq!(
            (status, code(&v)),
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
        );
    }

    let data = s.dir.path().to_owned();
    s.api.set_registry(
        tokio::task::spawn_blocking(move || registry(&data))
            .await
            .unwrap(),
    );
    assert_eq!(s.get("/readyz").await.0, StatusCode::OK);
    let v = s.ok(Method::GET, "/v1", None).await;
    assert_eq!(v["name"], "nocturnum");
    assert_eq!(v["role"], "anonymous");
    assert_eq!(v["keys"], json!({"admin": false, "read_only": false}));
    assert!(v["uptime_seconds"].as_u64().is_some());
    assert!(v["started_at"].as_str().unwrap().ends_with('Z'));
    let disk = &v["disk"]["collections"];
    if !disk.is_null() {
        assert!(disk["available_bytes"].as_u64().unwrap() <= disk["total_bytes"].as_u64().unwrap());
    }
}

#[tokio::test]
async fn collections_lifecycle() {
    let s = start().await;
    s.create("notes", 3, "cosine").await;
    let again = json!({"vectors": {"dim": 3, "distance": "cosine"}});
    assert_eq!(
        s.put("/v1/collections/notes", again).await,
        (StatusCode::OK, json!({"created": false}))
    );
    let (status, v) = s
        .put(
            "/v1/collections/notes",
            json!({"vectors": {"dim": 4, "distance": "cosine"}}),
        )
        .await;
    assert_eq!((status, code(&v)), (StatusCode::CONFLICT, "conflict"));
    for bad in ["Notes", "_x", "a.b"] {
        let (status, v) = s
            .put(
                &format!("/v1/collections/{bad}"),
                json!({"vectors": {"dim": 3, "distance": "dot"}}),
            )
            .await;
        assert_eq!(
            (status, code(&v)),
            (StatusCode::BAD_REQUEST, "invalid_request"),
            "{bad}"
        );
    }
    let (status, v) = s
        .put(
            "/v1/collections/zero",
            json!({"vectors": {"dim": 0, "distance": "dot"}}),
        )
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );

    s.create("other", 2, "dot").await;
    let v = s.ok(Method::GET, "/v1/collections", None).await;
    assert_eq!(
        v,
        json!({"collections": [
            {"name": "notes", "points": 0, "vectors": {"dim": 3, "distance": "cosine"},
             "segments": 0, "failure": null},
            {"name": "other", "points": 0, "vectors": {"dim": 2, "distance": "dot"},
             "segments": 0, "failure": null},
        ]})
    );
    let v = s.ok(Method::GET, "/v1/collections/notes", None).await;
    assert_eq!(
        v,
        json!({"name": "notes", "vectors": {"dim": 3, "distance": "cosine"}, "points": 0,
               "segments": 0, "indexes": []})
    );
    let v = s.ok(Method::DELETE, "/v1/collections/notes", None).await;
    assert_eq!(v, json!({"deleted": true}));
    let (status, v) = s.call(Method::DELETE, "/v1/collections/notes", None).await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    assert_eq!(
        s.get("/v1/collections/notes").await.0,
        StatusCode::NOT_FOUND
    );
    // The name can be used again, with a new config.
    s.create("notes", 5, "euclid").await;
}

#[tokio::test]
async fn points_round_trip() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    let big = "18446744073709551615";
    let v = s
        .put(
            "/v1/collections/c/points?ack=applied",
            json!({"points": [
                {"id": 1, "vector": [1.0, 0.0], "payload": {"tags": ["red"], "price": 10}},
                {"id": 2, "vector": [0.9, 0.1], "payload": {"tags": ["blue"], "price": 30}},
                {"id": big, "vector": [0.0, 1.0], "payload": {"tags": ["red"], "price": 5}},
                {"id": "550E8400-E29B-41D4-A716-446655440000", "vector": [0.5, 0.5]},
            ]}),
        )
        .await;
    assert_eq!(v.0, StatusCode::OK, "{}", v.1);
    assert_eq!(v.1["ack"], "applied");
    let seq = v.1["seq"].as_u64().unwrap();

    // Get one, with the defaults: payload yes, vector no.
    let v = s.ok(Method::GET, "/v1/collections/c/points/1", None).await;
    assert_eq!(
        v,
        json!({"id": 1, "payload": {"tags": ["red"], "price": 10}})
    );
    let v = s
        .ok(
            Method::GET,
            &format!("/v1/collections/c/points/{big}?with_payload=false&with_vector=true"),
            None,
        )
        .await;
    assert_eq!(v, json!({"id": big, "vector": [0.0, 1.0]}));
    let (status, v) = s.get("/v1/collections/c/points/7").await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, _) = s.get("/v1/collections/c/points/-1").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Get many keeps the order asked for and lowercases UUIDs.
    let v = s
        .post(
            "/v1/collections/c/points/get",
            json!({"ids": ["550e8400-e29b-41d4-a716-446655440000", 2, 99, 2],
                   "with_payload": {"include": ["price"]}}),
        )
        .await
        .1;
    assert_eq!(
        v,
        json!({"points": [
            {"id": "550e8400-e29b-41d4-a716-446655440000", "payload": {}},
            {"id": 2, "payload": {"price": 30}},
        ]})
    );

    // Scroll pages in id order, with large ids as strings.
    let p1 = s
        .post(
            "/v1/collections/c/points/scroll",
            json!({"limit": 2, "with_payload": false}),
        )
        .await
        .1;
    assert_eq!(p1, json!({"points": [{"id": 1}, {"id": 2}], "next": 2}));
    let p2 = s
        .post(
            "/v1/collections/c/points/scroll",
            json!({"limit": 2, "after": 2, "with_payload": false}),
        )
        .await
        .1;
    assert_eq!(
        ids(&p2),
        [json!(big), json!("550e8400-e29b-41d4-a716-446655440000")]
    );
    assert_eq!(p2["next"], Value::Null);
    let red = json!({"must": [{"key": "tags", "match": "red"}]});
    let v = s
        .post(
            "/v1/collections/c/points/scroll",
            json!({"filter": red, "with_payload": {"exclude": ["tags"]}}),
        )
        .await
        .1;
    assert_eq!(
        v,
        json!({"points": [{"id": 1, "payload": {"price": 10}}, {"id": big, "payload": {"price": 5}}],
               "next": null})
    );

    // Count, with and without a filter, and with an empty body.
    assert_eq!(s.count(json!({})).await, json!({"count": 4}));
    assert_eq!(s.count(json!({"filter": red})).await, json!({"count": 2}));
    let resp = s
        .request(Method::POST, "/v1/collections/c/points/count")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.json::<Value>().await.unwrap(), json!({"count": 4}));

    // Search, best first, with offset, threshold and filter.
    let search = |body: Value| async { s.post("/v1/collections/c/points/search", body).await };
    let (status, v) = search(json!({"vector": [1.0, 0.0], "limit": 2})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&v), [json!(1), json!(2)]);
    assert_eq!(v["points"][0]["score"], json!(1.0));
    assert_eq!(v["points"][0]["payload"]["price"], json!(10));
    let v = search(json!({"vector": [1.0, 0.0], "limit": 2, "offset": 1, "with_payload": false}))
        .await
        .1;
    assert_eq!(
        v["points"],
        json!([{"id": 2, "score": 0.9}, {"id": "550e8400-e29b-41d4-a716-446655440000", "score": 0.5}])
    );
    let v = search(json!({"vector": [1.0, 0.0], "score_threshold": 0.7}))
        .await
        .1;
    assert_eq!(ids(&v), [json!(1), json!(2)]);
    let v = search(json!({"vector": [0.0, 1.0], "filter": red, "exact": true, "hnsw_ef": 16, "with_vector": true}))
        .await
        .1;
    assert_eq!(ids(&v), [json!(big), json!(1)]);
    assert_eq!(v["points"][0]["vector"], json!([0.0, 1.0]));

    // Payload changes, by ids and by filter.
    let s1 = s
        .write(
            "payload/set",
            json!({"ids": [1], "payload": {"note": "hi"}}),
        )
        .await;
    assert!(s1 > seq);
    s.write("payload/overwrite", json!({"filter": {"must": [{"key": "price", "range": {"gte": 30}}]}, "payload": {"only": true}})).await;
    s.write("payload/delete", json!({"ids": [1], "keys": ["tags"]}))
        .await;
    s.write("payload/clear", json!({"ids": [big]})).await;
    let v = s
        .post("/v1/collections/c/points/get", json!({"ids": [1, 2, big]}))
        .await
        .1;
    assert_eq!(
        v["points"],
        json!([
            {"id": 1, "payload": {"price": 10, "note": "hi"}},
            {"id": 2, "payload": {"only": true}},
            {"id": big, "payload": {}},
        ])
    );

    // Deletes, by filter and by ids.
    s.write(
        "delete",
        json!({"filter": {"must": [{"key": "only", "match": true}]}}),
    )
    .await;
    s.write("delete", json!({"ids": [1, 12345]})).await;
    assert_eq!(s.count(json!({})).await, json!({"count": 2}));

    // Just past 2^53 is where JavaScript starts losing digits.
    s.put(
        "/v1/collections/c/points",
        json!({"points": [{"id": 9_007_199_254_740_993u64, "vector": [1.0, 1.0]}]}),
    )
    .await;
    let v = s
        .ok(
            Method::GET,
            "/v1/collections/c/points/9007199254740993",
            None,
        )
        .await;
    assert_eq!(v["id"], "9007199254740993");

    // One read returns at most as many points as a scroll page.
    let many: Vec<u64> = (0..10_001).collect();
    let (status, v) = s
        .post("/v1/collections/c/points/get", json!({"ids": many}))
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    assert_eq!(v["error"]["at"], "ids");
}

#[tokio::test]
async fn errors_say_what_and_where() {
    let s = start_with(
        ApiOptions {
            max_body_bytes: 4096,
            ..ApiOptions::default()
        },
        true,
    )
    .await;
    s.create("c", 2, "cosine").await;
    let check = |r: (StatusCode, Value), status: StatusCode, c: &str, at: Option<&str>| {
        assert_eq!(r.0, status, "{}", r.1);
        assert_eq!(code(&r.1), c, "{}", r.1);
        assert_eq!(r.1["error"]["at"].as_str(), at, "{}", r.1);
    };
    let bad = StatusCode::BAD_REQUEST;

    let r = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [{"id": 1, "vector": [1.0, 0.0]}, {"id": 2, "vector": [0.0, 0.0]}]}),
        )
        .await;
    check(r, bad, "invalid_vector", Some("points[1].vector"));
    // Nothing from a refused request was written.
    assert_eq!(
        s.post("/v1/collections/c/points/count", json!({})).await.1,
        json!({"count": 0})
    );
    let r = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [{"id": -3, "vector": [1.0, 0.0]}]}),
        )
        .await;
    check(r, bad, "invalid_request", Some("points[0].id"));
    let r = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [], "wait": true}),
        )
        .await;
    check(r, bad, "invalid_request", Some("wait"));
    let mut deep = json!(1);
    for _ in 0..70 {
        deep = json!({ "a": deep });
    }
    let r = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [{"id": 1, "vector": [1.0, 0.0], "payload": deep}]}),
        )
        .await;
    check(r, bad, "invalid_payload", Some("points[0].payload"));
    let r = s
        .post(
            "/v1/collections/c/points/search",
            json!({"vector": [1.0, 0.0, 0.0]}),
        )
        .await;
    check(r, bad, "invalid_vector", Some("vector"));
    let r = s
        .post(
            "/v1/collections/c/points/count",
            json!({"filter": {"must": [{"key": "a", "match": 1}, {"key": "b", "any": []}]}}),
        )
        .await;
    check(r, bad, "invalid_filter", Some("filter.must[1]"));
    let r = s
        .post(
            "/v1/collections/c/points/search",
            json!({"vector": [1.0, 0.0], "limit": 10_001}),
        )
        .await;
    check(r, bad, "invalid_request", None);
    let r = s
        .post(
            "/v1/collections/c/points/delete",
            json!({"ids": [1], "filter": {}}),
        )
        .await;
    check(r, bad, "invalid_request", None);
    let r = s.post("/v1/collections/c/points/delete", json!({})).await;
    check(r, bad, "invalid_request", None);
    let r = s
        .post(
            "/v1/collections/c/points/delete?ack=always",
            json!({"ids": [1]}),
        )
        .await;
    check(r, bad, "invalid_request", None);
    let r = s.post("/v1/collections/nope/points/count", json!({})).await;
    check(r, StatusCode::NOT_FOUND, "not_found", None);
    let r = s.get("/v1/nothing/here").await;
    check(r, StatusCode::NOT_FOUND, "not_found", None);
    let r = s.call(Method::PATCH, "/v1/collections/c", None).await;
    check(
        r,
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        None,
    );

    // A body has to say it is JSON.
    let resp = s
        .request(Method::POST, "/v1/collections/c/points/count")
        .header("content-type", "text/plain")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let resp = s
        .request(Method::POST, "/v1/collections/c/points/count")
        .header("content-type", "application/json")
        .body("{\"filter\": ")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), bad);
    // And fit under the limit.
    let big = vec![0.5f32; 2000];
    let r = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [{"id": 1, "vector": big}]}),
        )
        .await;
    check(r, StatusCode::PAYLOAD_TOO_LARGE, "too_large", None);
}

#[tokio::test]
async fn keys_guard_everything_but_health() {
    let mut s = start_with_keys().await;
    s.create("c", 2, "dot").await;
    let point = json!({"points": [{"id": 1, "vector": [1.0, 0.0]}]});
    assert_eq!(
        s.put("/v1/collections/c/points", point.clone()).await.0,
        StatusCode::OK
    );

    s.key = None;
    assert_eq!(s.get("/livez").await.0, StatusCode::OK);
    assert_eq!(s.get("/readyz").await.0, StatusCode::OK);
    for path in ["/v1", "/v1/collections", "/metrics"] {
        let resp = s.request(Method::GET, path).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{path}");
        assert_eq!(resp.headers()["www-authenticate"], "Bearer");
        let v: Value = resp.json().await.unwrap();
        assert_eq!(code(&v), "unauthorized");
    }
    // The key is checked before the body is read, so a stranger cannot make the server take
    // in a large upload.
    // The request declares a 64 MiB body and sends none of it, so only a server that answers
    // before reading the body can reply in time.
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let addr = s.base.trim_start_matches("http://");
        let mut conn = tokio::net::TcpStream::connect(addr).await.unwrap();
        let head = format!(
            "PUT /v1/collections/c/points HTTP/1.1\r\nHost: {addr}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            64 * 1024 * 1024
        );
        conn.write_all(head.as_bytes()).await.unwrap();
        // Reads through the end of the status line, which may arrive in pieces.
        let read_status = async {
            let mut reply = Vec::new();
            let mut buf = [0u8; 256];
            while !reply.windows(2).any(|w| w == b"\r\n") && reply.len() < 4096 {
                let n = conn.read(&mut buf).await.unwrap();
                assert!(n > 0, "the server closed without answering");
                reply.extend_from_slice(&buf[..n]);
            }
            reply
        };
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), read_status)
            .await
            .expect("the server answers without waiting for the body");
        let reply = String::from_utf8_lossy(&reply);
        assert!(reply.starts_with("HTTP/1.1 401"), "{reply}");
    }
    s.key = Some("example-wrong-key-0123");
    assert_eq!(s.get("/v1/collections").await.0, StatusCode::UNAUTHORIZED);

    s.key = Some(ADMIN);
    assert_eq!(s.ok(Method::GET, "/v1", None).await["role"], "admin");
    s.key = Some(READER);
    let v = s.ok(Method::GET, "/v1", None).await;
    assert_eq!(v["role"], "read-only");
    assert_eq!(v["keys"], json!({"admin": true, "read_only": true}));
    assert_eq!(s.get("/v1/collections/c").await.0, StatusCode::OK);
    assert_eq!(s.get("/v1/collections/c/status").await.0, StatusCode::OK);
    assert_eq!(s.get("/metrics").await.0, StatusCode::OK);
    for path in ["search", "scroll", "count", "get", "recall"] {
        let body = match path {
            "search" => json!({"vector": [1.0, 0.0]}),
            "get" => json!({"ids": [1]}),
            _ => json!({}),
        };
        let (status, v) = s
            .post(&format!("/v1/collections/c/points/{path}"), body)
            .await;
        assert_eq!(status, StatusCode::OK, "{path}: {v}");
    }
    let refused = [
        (Method::PUT, "/v1/collections/c/points", point.clone()),
        (
            Method::POST,
            "/v1/collections/c/points/delete",
            json!({"ids": [1]}),
        ),
        (
            Method::PUT,
            "/v1/collections/d",
            json!({"vectors": {"dim": 2, "distance": "dot"}}),
        ),
        (Method::DELETE, "/v1/collections/c", json!({})),
        (Method::POST, "/v1/collections/c/snapshots", json!({})),
        (
            Method::PUT,
            "/v1/collections/c/indexes",
            json!({"path": "a", "kind": "keyword"}),
        ),
    ];
    for (method, path, body) in refused {
        let (status, v) = s.call(method, path, Some(body)).await;
        assert_eq!(
            (status, code(&v)),
            (StatusCode::FORBIDDEN, "forbidden"),
            "{path}"
        );
    }
    s.key = Some(ADMIN);
    assert_eq!(
        s.post("/v1/collections/c/points/count", json!({})).await.1,
        json!({"count": 1})
    );
}

#[tokio::test]
async fn without_keys_only_known_hosts_are_served() {
    let s = start_with(
        ApiOptions {
            allowed_hosts: vec!["my-server".into()],
            ..ApiOptions::default()
        },
        true,
    )
    .await;
    let with_host = |host: &str| {
        s.request(Method::GET, "/v1/collections")
            .header("host", host)
            .send()
    };
    for host in [
        "localhost:6333",
        "127.0.0.1",
        "[::1]:6333",
        "my-server:6333",
        "MY-SERVER",
    ] {
        assert_eq!(
            with_host(host).await.unwrap().status(),
            StatusCode::OK,
            "{host}"
        );
    }
    for host in [
        "evil.example",
        "evil.example:6333",
        "localhost.evil.example",
    ] {
        let resp = with_host(host).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{host}");
    }
    // A page on another site cannot change data, even with a request a browser sends
    // without asking first.
    s.create("c", 2, "dot").await;
    let from = |origin: String| {
        s.request(Method::POST, "/v1/collections/c/snapshots")
            .header("origin", origin)
            .send()
    };
    let port = s.base.rsplit(':').next().unwrap().to_owned();
    for evil in [
        "http://evil.example".to_owned(),
        "null".to_owned(),
        format!("http://127.0.0.1:{}", port.parse::<u32>().unwrap() + 1),
    ] {
        let resp = from(evil.clone()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{evil}");
    }
    assert_eq!(from(s.base.clone()).await.unwrap().status(), StatusCode::OK);
    let resp = s
        .request(Method::POST, "/v1/collections/c/snapshots")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "clients that are not browsers send no origin"
    );
    let resp = s
        .request(Method::GET, "/v1/collections")
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reads are left to the browser's own rules"
    );

    // Health checks answer whatever the host.
    let resp = s
        .request(Method::GET, "/livez")
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // With keys set the key is what matters.
    let k = start_with_keys().await;
    let resp = k
        .request(Method::GET, "/v1/collections")
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    k.create("c", 2, "dot").await;
    let resp = k
        .request(Method::POST, "/v1/collections/c/snapshots")
        .header("origin", "http://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn payload_indexes() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    let spec = json!({"path": "tags", "kind": "keyword"});
    assert_eq!(
        s.put("/v1/collections/c/indexes", spec.clone()).await.1,
        json!({"created": true})
    );
    assert_eq!(
        s.put("/v1/collections/c/indexes", spec.clone()).await.1,
        json!({"created": false})
    );
    let (status, v) = s
        .put(
            "/v1/collections/c/indexes",
            json!({"path": "tags", "kind": "geo"}),
        )
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    assert_eq!(
        s.get("/v1/collections/c/indexes").await.1,
        json!({"indexes": [spec]})
    );
    assert_eq!(
        s.get("/v1/collections/c").await.1["indexes"],
        json!([{"path": "tags", "kind": "keyword"}])
    );
    assert_eq!(
        s.post("/v1/collections/c/indexes/delete", spec.clone())
            .await
            .1,
        json!({"deleted": true})
    );
    assert_eq!(
        s.post("/v1/collections/c/indexes/delete", spec).await.1,
        json!({"deleted": false})
    );
}

#[tokio::test]
async fn text_search_ranks_by_bm25() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    let points = json!({"points": [
        {"id": 1, "vector": [0.0, 1.0], "payload": {"body": "The red fox", "n": 1}},
        {"id": 2, "vector": [1.0, 0.0], "payload": {"body": ["red hen", "red barn"], "n": 2}},
        {"id": 3, "vector": [1.0, 1.0], "payload": {"body": "a quiet night", "n": 3}}
    ]});
    assert_eq!(
        s.put("/v1/collections/c/points", points).await.0,
        StatusCode::OK
    );
    let search = |body: Value| async { s.post("/v1/collections/c/points/search", body).await };

    let (status, v) = search(json!({"text": {"field": "body", "query": "red"}})).await;
    assert_eq!(
        (status, code(&v), v["error"]["at"].as_str()),
        (
            StatusCode::BAD_REQUEST,
            "invalid_request",
            Some("text.field")
        )
    );
    let index = json!({"path": "body", "kind": "text"});
    assert_eq!(
        s.put("/v1/collections/c/indexes", index).await.1,
        json!({"created": true})
    );

    let (status, v) =
        search(json!({"text": {"field": "body", "query": "Red"}, "with_payload": false})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(&v), [json!(2), json!(1)]);
    assert!(v["points"][0]["score"].as_f64().unwrap() > v["points"][1]["score"].as_f64().unwrap());
    let (_, v) = search(json!({
        "text": {"field": "body", "query": "red", "k1": 2.0, "b": 0.0},
        "filter": {"must": [{"key": "n", "match": 1}]}
    }))
    .await;
    assert_eq!(ids(&v), [json!(1)]);
    assert_eq!(v["points"][0]["payload"]["body"], "The red fox");

    let bad = StatusCode::BAD_REQUEST;
    let (status, v) = search(json!({"text": {"field": "body", "query": "red", "b": 2}})).await;
    assert_eq!(
        (status, code(&v), v["error"]["at"].as_str()),
        (bad, "invalid_request", Some("text.b"))
    );
    let (status, v) =
        search(json!({"vector": [1.0, 0.0], "text": {"field": "body", "query": "red"}})).await;
    assert_eq!((status, code(&v)), (bad, "invalid_request"));
    let (status, v) = search(json!({"limit": 3})).await;
    assert_eq!((status, code(&v)), (bad, "invalid_request"));
    let (status, v) = search(json!({"text": {"field": "body", "query": "red", "boost": 2}})).await;
    assert_eq!((status, code(&v)), (bad, "invalid_request"));
    let many: Vec<String> = (0..1025).map(|i| format!("w{i}")).collect();
    let (status, v) = search(json!({"text": {"field": "body", "query": many.join(" ")}})).await;
    assert_eq!(
        (status, code(&v), v["error"]["at"].as_str()),
        (bad, "invalid_request", Some("text.query"))
    );
    // hnsw_ef only matters to vector searches.
    let (status, _) =
        search(json!({"text": {"field": "body", "query": "red"}, "hnsw_ef": 0})).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn snapshots_download_and_restore() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    let points = json!({"points": [{"id": 1, "vector": [1.0, 0.0], "payload": {"v": 1}}]});
    s.put("/v1/collections/c/points", points).await;

    let snap = s
        .ok(Method::POST, "/v1/collections/c/snapshots", None)
        .await;
    let name = snap["name"].as_str().unwrap().to_owned();
    assert!(
        name.starts_with("c-") && name.ends_with(".snapshot"),
        "{name}"
    );
    assert!(snap["created_at"].as_str().unwrap().ends_with('Z'));
    let list = s.ok(Method::GET, "/v1/collections/c/snapshots", None).await;
    assert_eq!(list, json!({"snapshots": [snap.clone()]}));

    // The download is the file the checksum describes.
    let resp = s
        .request(Method::GET, &format!("/v1/collections/c/snapshots/{name}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "application/x-tar");
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(bytes.len() as u64, snap["size"].as_u64().unwrap());
    use sha2::Digest;
    let sum: String = sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(sum, snap["sha256"]);

    // Names that are not plain snapshot files are never opened.
    for bad in ["..%2F..%2Fx.snapshot", "c.tar", "x.snapshot"] {
        let (status, _) = s.get(&format!("/v1/collections/c/snapshots/{bad}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{bad}");
    }

    // Restoring replaces later changes.
    let more = json!({"points": [{"id": 2, "vector": [0.0, 1.0]}]});
    s.put("/v1/collections/c/points", more).await;
    s.post(
        "/v1/collections/c/points/payload/set",
        json!({"ids": [1], "payload": {"v": 2}}),
    )
    .await;
    let v = s
        .ok(
            Method::POST,
            "/v1/collections/c/restore",
            Some(json!({"snapshot": name})),
        )
        .await;
    assert_eq!(v, json!({"restored": true}));
    let v = s.post("/v1/collections/c/points/scroll", json!({})).await.1;
    assert_eq!(
        v,
        json!({"points": [{"id": 1, "payload": {"v": 1}}], "next": null})
    );
    // And the restored collection takes writes.
    let (status, _) = s
        .put(
            "/v1/collections/c/points",
            json!({"points": [{"id": 3, "vector": [1.0, 1.0]}]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // Snapshots outlive their collection, and restoring brings it back.
    s.ok(Method::DELETE, "/v1/collections/c", None).await;
    assert_eq!(
        s.ok(Method::GET, "/v1/collections/c/snapshots", None).await,
        json!({"snapshots": [snap]})
    );
    s.ok(
        Method::POST,
        "/v1/collections/c/restore",
        Some(json!({"snapshot": name})),
    )
    .await;
    assert_eq!(
        s.post("/v1/collections/c/points/count", json!({})).await.1,
        json!({"count": 1})
    );

    let (status, v) = s
        .post(
            "/v1/collections/c/restore",
            json!({"snapshot": "c-000000000000000.snapshot"}),
        )
        .await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    s.ok(
        Method::DELETE,
        &format!("/v1/collections/c/snapshots/{name}"),
        None,
    )
    .await;
    assert_eq!(
        s.ok(Method::GET, "/v1/collections/c/snapshots", None).await,
        json!({"snapshots": []})
    );
    assert_eq!(
        s.get("/v1/collections/zz/snapshots").await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_damaged_snapshot_is_refused_and_the_collection_kept() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    s.put(
        "/v1/collections/c/points",
        json!({"points": [{"id": 1, "vector": [1.0, 0.0]}]}),
    )
    .await;
    let snap = s
        .ok(Method::POST, "/v1/collections/c/snapshots", None)
        .await;
    let name = snap["name"].as_str().unwrap();
    let path = s.dir.path().join("snapshots/c").join(name);
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&path, bytes).unwrap();
    let (status, v) = s
        .post("/v1/collections/c/restore", json!({"snapshot": name}))
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_snapshot")
    );
    assert_eq!(
        s.post("/v1/collections/c/points/count", json!({})).await.1,
        json!({"count": 1})
    );
}

#[tokio::test]
async fn a_failed_fsync_makes_the_server_unready() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    let point = json!({"points": [{"id": 1, "vector": [1.0, 0.0]}]});
    assert_eq!(
        s.put("/v1/collections/c/points", point.clone()).await.0,
        StatusCode::OK
    );
    s.api
        .registry()
        .unwrap()
        .get("c")
        .unwrap()
        .inject_sync_failures(1);
    let (status, v) = s.put("/v1/collections/c/points", point.clone()).await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{v}"
    );
    let (status, v) = s.put("/v1/collections/c/points", point).await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{v}"
    );
    let (status, v) = s.get("/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("collection c"),
        "{v}"
    );
    let metrics = s.get("/metrics").await.1;
    assert!(
        metrics
            .as_str()
            .unwrap()
            .contains("nocturnum_collection_failed{collection=\"c\"} 1")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_failed_manifest_write_stops_flushing_but_not_writes() {
    use std::os::unix::fs::PermissionsExt;
    let s = start().await;
    s.create("c", 2, "dot").await;
    let dir = s.dir.path().join("collections/c");
    let mode = |m| std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(m)).unwrap();
    let index = json!({"path": "a", "kind": "keyword"});
    mode(0o555);
    let (status, v) = s.put("/v1/collections/c/indexes", index.clone()).await;
    mode(0o755);
    assert_eq!(
        (status, code(&v)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{v}"
    );
    // Writes still land in the WAL, while the server reports it needs a restart.
    let point = json!({"points": [{"id": 1, "vector": [1.0, 0.0]}]});
    assert_eq!(
        s.put("/v1/collections/c/points", point).await.0,
        StatusCode::OK
    );
    assert_eq!(s.get("/readyz").await.0, StatusCode::SERVICE_UNAVAILABLE);
    let (status, v) = s.put("/v1/collections/c/indexes", index).await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{v}"
    );
    let (status, v) = s
        .call(Method::POST, "/v1/collections/c/snapshots", None)
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
        "{v}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_collection_lost_by_a_failed_delete_makes_the_server_unready() {
    use std::os::unix::fs::PermissionsExt;
    let s = start().await;
    s.create("c", 2, "dot").await;
    let cols = s.dir.path().join("collections");
    let mode =
        |p: &Path, m| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    // The rename fails on a read-only parent, and reopening fails on an unreadable copy.
    mode(&cols.join("c"), 0o000);
    mode(&cols, 0o555);
    let (status, v) = s.call(Method::DELETE, "/v1/collections/c", None).await;
    mode(&cols, 0o755);
    mode(&cols.join("c"), 0o755);
    assert_eq!(
        (status, code(&v)),
        (StatusCode::INTERNAL_SERVER_ERROR, "internal")
    );
    let (status, v) = s.get("/readyz").await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("collection c"),
        "{v}"
    );
}

#[tokio::test]
async fn metrics_label_routes_and_collections() {
    let s = start().await;
    s.create("c", 2, "dot").await;
    s.put(
        "/v1/collections/c/points",
        json!({"points": [{"id": 1, "vector": [1.0, 0.0]}]}),
    )
    .await;
    s.get("/v1/collections/missing").await;
    s.get("/no/such/path").await;
    let resp = s.request(Method::GET, "/metrics").send().await.unwrap();
    assert!(
        resp.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let text = resp.text().await.unwrap();
    for line in [
        "nocturnum_http_requests_total{method=\"PUT\",route=\"/v1/collections/{name}/points\",status=\"200\"} 1",
        "nocturnum_http_requests_total{method=\"GET\",route=\"/v1/collections/{name}\",status=\"404\"} 1",
        "nocturnum_http_requests_total{method=\"GET\",route=\"unmatched\",status=\"404\"} 1",
        "nocturnum_collection_points{collection=\"c\"} 1",
        "nocturnum_wal_last_seq{collection=\"c\"} 1",
        "nocturnum_ready 1",
    ] {
        assert!(text.contains(line), "missing {line} in\n{text}");
    }
    assert!(text.contains("nocturnum_wal_fsyncs_total{collection=\"c\"}"));
}

// The registry on its own.

fn upsert_one(r: &Registry, name: &str, id: u64) {
    let c = r.get(name).unwrap();
    c.upsert(
        vec![nocturnum_storage::PointWrite {
            id: id.into(),
            vector: vec![1.0, 0.0],
            payload: Default::default(),
        }],
        nocturnum_core::Ack::Durable,
    )
    .unwrap();
}

fn dot2() -> nocturnum_core::VectorConfig {
    nocturnum_core::VectorConfig {
        dim: 2,
        distance: nocturnum_core::Distance::Dot,
    }
}

#[test]
fn collections_survive_a_restart() {
    let dir = TempDir::new().unwrap();
    let r = registry(dir.path());
    assert!(r.create("a", dot2()).unwrap());
    upsert_one(&r, "a", 7);
    // A second server cannot share the directory.
    let second = Registry::open(dir.path(), RegistryOptions::default());
    assert!(second.is_err());
    drop(r);
    let r = registry(dir.path());
    assert_eq!(r.get("a").unwrap().len(), 1);
}

#[test]
fn startup_settles_what_a_crash_left() {
    let dir = TempDir::new().unwrap();
    let cols = dir.path().join("collections");
    let r = registry(dir.path());
    for name in ["a", "b"] {
        r.create(name, dot2()).unwrap();
    }
    upsert_one(&r, "a", 1);
    let snap = r.take_snapshot("a").unwrap();
    let snap_path = dir.path().join("snapshots/a").join(&snap.name);
    upsert_one(&r, "a", 2);
    drop(r);

    // A restore of a that had moved the old copy aside: the restored copy is finished.
    nocturnum_storage::shard::restore(&snap_path, &cols.join(".restoring-a")).unwrap();
    std::fs::rename(cols.join("a"), cols.join(".replaced-a")).unwrap();
    // A restore of b that had moved the old copy aside but lost the new one is undone.
    std::fs::rename(cols.join("b"), cols.join(".replaced-b")).unwrap();
    // Leftovers of a create, a delete, and a restore that never got as far as the swap.
    for junk in [".creating-x", ".deleted-y", ".restoring-zz"] {
        std::fs::create_dir_all(cols.join(junk).join("inner")).unwrap();
    }

    let r = registry(dir.path());
    assert_eq!(r.get("a").unwrap().len(), 1, "the restore was finished");
    assert!(r.get("b").is_ok(), "the old copy was put back");
    let names: Vec<String> = std::fs::read_dir(&cols)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    let mut names = names;
    names.sort();
    assert_eq!(names, ["a", "b"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_schedules() {
    let mut s = start_with_keys().await;
    s.create("c", 2, "dot").await;
    let path = "/v1/collections/c/snapshots/schedule";

    let (status, v) = s.get(path).await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
    let (status, v) = s.put(path, json!({"cron": "0 3 * * *", "keep": 0})).await;
    assert_eq!(
        (status, v["error"]["at"].as_str()),
        (StatusCode::BAD_REQUEST, Some("keep"))
    );
    let (status, v) = s.put(path, json!({"cron": "0 25 * * *", "keep": 7})).await;
    assert_eq!(
        (status, v["error"]["at"].as_str()),
        (StatusCode::BAD_REQUEST, Some("cron"))
    );
    let (status, v) = s
        .put(path, json!({"cron": "0 3 * * *", "keep": 7, "tz": "x"}))
        .await;
    assert_eq!(
        (status, code(&v)),
        (StatusCode::BAD_REQUEST, "invalid_request")
    );
    let (status, _) = s
        .put(
            "/v1/collections/nope/snapshots/schedule",
            json!({"cron": "@daily", "keep": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let set = s
        .ok(
            Method::PUT,
            path,
            Some(json!({"cron": "0 3 * * *", "keep": 7})),
        )
        .await;
    assert_eq!(set["cron"], "0 3 * * *");
    assert_eq!(set["keep"], 7);
    assert!(
        set["next_run"].as_str().unwrap().ends_with("T03:00:00Z"),
        "{set}"
    );
    assert_eq!(set["running"], false);
    assert_eq!(set["damaged"], json!([]));

    // The read-only key can look but not change it.
    s.key = Some(READER);
    assert_eq!(s.ok(Method::GET, path, None).await, set);
    let (status, _) = s.put(path, json!({"cron": "@hourly", "keep": 1})).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = s.call(Method::DELETE, path, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    s.key = Some(ADMIN);

    let (_, metrics) = s.get("/metrics").await;
    let metrics = metrics.as_str().unwrap();
    assert!(
        metrics.contains("nocturnum_snapshot_running{collection=\"c\"} 0"),
        "{metrics}"
    );
    assert!(metrics.contains("nocturnum_snapshot_next_run_timestamp_seconds{collection=\"c\"}"));
    assert!(metrics.contains("nocturnum_snapshot_failures_total{collection=\"c\"} 0"));

    // A snapshot name never shadows the schedule, and the schedule never lists as a snapshot.
    s.ok(Method::POST, "/v1/collections/c/snapshots", None)
        .await;
    let list = s.ok(Method::GET, "/v1/collections/c/snapshots", None).await;
    assert_eq!(list["snapshots"].as_array().unwrap().len(), 1);

    let v = s.ok(Method::DELETE, path, None).await;
    assert_eq!(v, json!({"deleted": true}));
    let v = s.ok(Method::DELETE, path, None).await;
    assert_eq!(v, json!({"deleted": false}));
    let (status, _) = s.get(path).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(s.dir.path().join("schedules.json").is_file());
}

#[tokio::test]
async fn status_and_recall() {
    let s = start().await;
    s.create("c", 2, "euclid").await;
    let (status, v) = s.get("/v1/collections/nope/status").await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));

    let points: Vec<Value> = (0..50)
        .map(|i| json!({"id": i, "vector": [(i % 10) as f32, (i / 10) as f32]}))
        .collect();
    let (status, v) = s
        .put("/v1/collections/c/points", json!({"points": points}))
        .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    let v = s.ok(Method::GET, "/v1/collections/c/status", None).await;
    assert_eq!(v["points"], 50);
    assert_eq!(v["failure"], Value::Null);
    assert_eq!(v["segments"], json!([]));
    assert_eq!(v["memtable"]["entries"], 50);
    assert_eq!(v["memtable"]["frozen_entries"], 0);
    assert_eq!(v["optimizer"]["progress"], Value::Null);
    assert!(v["wal"]["last_seq"].as_u64().unwrap() >= 1);
    assert_eq!(v["index"]["m"], 16);
    assert_eq!(v["index"]["default_ef"], 128);
    // The test registry runs no optimizer.
    assert_eq!(v["optimizer"]["enabled"], false);
    assert_eq!(v["optimizer"]["vacuum_ratio"], Value::Null);

    let v = s
        .ok(
            Method::POST,
            "/v1/collections/c/points/recall",
            Some(json!({"samples": 20, "limit": 3, "seed": 1})),
        )
        .await;
    assert_eq!(
        (v["samples"].as_u64(), v["limit"].as_u64()),
        (Some(20), Some(3))
    );
    assert_eq!(
        (v["points"].as_u64(), v["indexed_points"].as_u64()),
        (Some(50), Some(0))
    );
    let runs = v["runs"].as_array().unwrap();
    assert_eq!(
        runs.iter()
            .map(|r| r["hnsw_ef"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![16, 32, 64, 128, 256]
    );
    // Nothing is under a graph yet, so every search is exact.
    for r in runs {
        assert_eq!(r["recall"], 1.0);
        assert!(r["p50_ms"].as_f64().unwrap() <= r["p99_ms"].as_f64().unwrap());
    }
    assert!(v["exact"]["p50_ms"].as_f64().is_some());

    let r = s.api.registry().unwrap();
    tokio::task::spawn_blocking(move || r.get("c").unwrap().flush().unwrap())
        .await
        .unwrap();
    let v = s.ok(Method::GET, "/v1/collections/c/status", None).await;
    let seg = &v["segments"][0];
    assert_eq!(
        (seg["rows"].as_u64(), seg["live"].as_u64()),
        (Some(50), Some(50))
    );
    // 50 two-dimensional points are far below the 10 MiB a graph needs.
    assert_eq!(seg["index"], "plain");
    assert_eq!(seg["graph_bytes"], 0);
    assert_eq!(seg["vector_bytes"], 50 * 2 * 4);
    assert_eq!(v["memtable"]["entries"], 0);
    assert!(v["disk_bytes"].as_u64().unwrap() >= seg["disk_bytes"].as_u64().unwrap());

    // Default widths are raised to the limit.
    let v = s
        .ok(
            Method::POST,
            "/v1/collections/c/points/recall",
            Some(json!({"samples": 2, "limit": 40})),
        )
        .await;
    let widths: Vec<u64> = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["hnsw_ef"].as_u64().unwrap())
        .collect();
    assert_eq!(widths, vec![40, 64, 128, 256]);

    for (body, at) in [
        (json!({"samples": 0}), None),
        (json!({"hnsw_ef": [2], "limit": 3}), None),
        (json!({"bogus": 1}), Some("bogus")),
    ] {
        let (status, v) = s
            .post("/v1/collections/c/points/recall", body.clone())
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}: {v}");
        if let Some(at) = at {
            assert_eq!(v["error"]["at"], at);
        }
    }
}

#[tokio::test]
async fn the_console_answers_outside_the_api_without_a_key() {
    use axum::response::IntoResponse;
    let dir = TempDir::new().unwrap();
    let api = Api::new(ApiOptions {
        admin_key: Some(ADMIN.into()),
        ..ApiOptions::default()
    });
    let data = dir.path().to_owned();
    api.set_registry(
        tokio::task::spawn_blocking(move || registry(&data))
            .await
            .unwrap(),
    );
    let console: nocturnum_api::Console =
        Arc::new(|_, uri, _| format!("console {}", uri.path()).into_response());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = api.router_with_console(Some(console));
    tokio::spawn(async move { axum::serve(listener, router).await });
    let client = reqwest::Client::new();
    let get = |path: &str| client.get(format!("{base}{path}")).send();

    for path in ["/", "/collections/notes", "/v1x", "/metricsz"] {
        let r = get(path).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        assert_eq!(r.text().await.unwrap(), format!("console {path}"));
    }
    // The API keeps its own answers, and still wants the key.
    for path in ["/v1", "/v1/collections", "/metrics"] {
        assert_eq!(
            get(path).await.unwrap().status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }
    for path in [
        "/v1/nope",
        "/v1%2Fcollections",
        "/%76%31/collections",
        "/V1/x",
        "/%6Detrics",
    ] {
        let r = get(path).await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
        let v: Value = r.json().await.unwrap();
        assert_eq!(code(&v), "not_found", "{path}");
    }
    assert_eq!(get("/livez").await.unwrap().status(), StatusCode::OK);
    // Without a console every unknown path is the API's 404.
    let s = start().await;
    let (status, v) = s.get("/collections/notes").await;
    assert_eq!((status, code(&v)), (StatusCode::NOT_FOUND, "not_found"));
}
