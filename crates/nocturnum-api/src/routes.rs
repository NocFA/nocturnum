//! The endpoints, each a thin call into a [`Collection`] or the [`Registry`].

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use nocturnum_core::{PayloadIndexSpec, PointId, SparseVector, VectorConfig};
use nocturnum_query::collection::MAX_RESULT_WINDOW;
use nocturnum_query::fusion::DEFAULT_RRF_K;
use nocturnum_query::{
    Bm25, Collection, FusedRequest, Fusion, Prefetch, Query, QueryError, RecallRequest,
    ScrollRequest, SearchRequest, SparseQuery, TextQuery, WithPayload,
};
use nocturnum_storage::{Job, PointWrite};
use serde::Serialize;
use serde_json::{Value, json};
use tokio_util::io::ReaderStream;

use crate::Api;
use crate::auth::Access;
use crate::dto::*;
use crate::error::ApiError;
use crate::extract::{Body, Params};
use crate::registry::Registry;

type Shared = State<Arc<Api>>;
type Reply = Result<Encoded, ApiError>;

/// A JSON response body, encoded on a blocking thread so a large one never holds up the
/// async workers that answer health checks.
pub(crate) struct Encoded(Vec<u8>);

impl IntoResponse for Encoded {
    fn into_response(self) -> Response {
        ([(header::CONTENT_TYPE, "application/json")], self.0).into_response()
    }
}

fn encode<T: Serialize>(value: &T) -> Result<Encoded, ApiError> {
    serde_json::to_vec(value)
        .map(Encoded)
        .map_err(|e| ApiError::internal(format!("encoding a response: {e}")))
}

/// Runs blocking work on tokio's blocking threads.
async fn run<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("request task failed: {e}")))?
}

/// Runs `f` on the named collection on a blocking thread and encodes its answer there.
async fn with_collection<T: Serialize>(
    api: &Api,
    name: String,
    f: impl FnOnce(&Collection) -> Result<T, ApiError> + Send + 'static,
) -> Reply {
    let registry = api.registry()?;
    run(move || encode(&f(&*registry.get(&name)?)?)).await
}

/// Runs `f` on the registry on a blocking thread.
async fn on_registry<T: Send + 'static>(
    api: &Api,
    f: impl FnOnce(&Registry) -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    let registry = api.registry()?;
    run(move || f(&registry)).await
}

/// Runs `f` on the registry on a blocking thread and encodes its answer there.
async fn with_registry<T: Serialize>(
    api: &Api,
    f: impl FnOnce(&Registry) -> Result<T, ApiError> + Send + 'static,
) -> Reply {
    let registry = api.registry()?;
    run(move || encode(&f(&registry)?)).await
}

pub(crate) fn routes() -> Router<Arc<Api>> {
    let c = "/v1/collections/{name}";
    Router::new()
        .route("/v1", get(version))
        .route("/metrics", get(metrics))
        .route("/v1/collections", get(list_collections))
        .route(
            c,
            put(create_collection)
                .get(describe_collection)
                .delete(delete_collection),
        )
        .route(&format!("{c}/points"), put(upsert))
        .route(&format!("{c}/points/delete"), post(delete_points))
        .route(&format!("{c}/points/get"), post(get_points))
        .route(&format!("{c}/points/scroll"), post(scroll))
        .route(&format!("{c}/points/count"), post(count))
        .route(&format!("{c}/points/search"), post(search))
        .route(&format!("{c}/points/query"), post(query))
        .route(&format!("{c}/points/recall"), post(recall))
        .route(&format!("{c}/status"), get(collection_status))
        .route(&format!("{c}/points/payload/set"), post(set_payload))
        .route(
            &format!("{c}/points/payload/overwrite"),
            post(overwrite_payload),
        )
        .route(&format!("{c}/points/payload/delete"), post(delete_keys))
        .route(&format!("{c}/points/payload/clear"), post(clear_payload))
        .route(&format!("{c}/points/{{id}}"), get(get_point))
        .route(&format!("{c}/indexes"), get(list_indexes).put(create_index))
        .route(&format!("{c}/indexes/delete"), post(delete_index))
        .route(
            &format!("{c}/snapshots"),
            post(take_snapshot).get(list_snapshots),
        )
        .route(
            &format!("{c}/snapshots/schedule"),
            get(get_schedule).put(set_schedule).delete(delete_schedule),
        )
        .route(
            &format!("{c}/snapshots/{{snapshot}}"),
            get(download_snapshot).delete(delete_snapshot),
        )
        .route(&format!("{c}/restore"), post(restore))
}

/// The health checks, which never need a key.
pub(crate) fn health() -> Router<Arc<Api>> {
    Router::new()
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route_layer(axum::middleware::from_fn(crate::metrics::tag))
}

pub(crate) async fn not_found() -> ApiError {
    ApiError::not_found("no such endpoint, see docs/api/v1.md")
}

pub(crate) async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "this endpoint does not take that method",
    )
}

// Health and metrics.

async fn livez() -> Json<Value> {
    Json(json!({"status": "ok"}))
}

async fn readyz(State(api): Shared) -> Response {
    let ready = on_registry(&api, |r| {
        if let Some((name, why)) = r.lost().into_iter().next() {
            return Err(ApiError::unavailable(format!("collection {name}: {why}")));
        }
        for (name, c) in r.list() {
            if let Some(why) = c.failure() {
                return Err(ApiError::unavailable(format!("collection {name}: {why}")));
            }
        }
        Ok(())
    })
    .await;
    match ready {
        Ok(()) => Json(json!({"status": "ready"})).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn metrics(State(api): Shared) -> Result<Response, ApiError> {
    let registry = api.registry.get().cloned();
    let api2 = api.clone();
    let text = run(move || Ok(api2.metrics.render(registry.as_deref()))).await?;
    Ok((
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        text,
    )
        .into_response())
}

fn disk(path: &std::path::Path) -> Value {
    match nocturnum_storage::disk_space(path) {
        Some(d) => json!({"total_bytes": d.total, "available_bytes": d.available}),
        None => Value::Null,
    }
}

async fn version(State(api): Shared, Extension(access): Extension<Access>) -> Reply {
    let (admin, read_only) = api.keys.configured();
    let started_at = crate::registry::rfc3339(api.started_at);
    let uptime = api.started.elapsed().as_secs();
    with_registry(&api, move |r| {
        Ok(json!({
            "name": "nocturnum",
            "version": env!("CARGO_PKG_VERSION"),
            "started_at": started_at,
            "uptime_seconds": uptime,
            "role": access.as_str(),
            "keys": {"admin": admin, "read_only": read_only},
            "disk": {
                "collections": disk(r.collections_dir()),
                "snapshots": disk(r.snapshots_dir()),
            },
        }))
    })
    .await
}

// Collections.

async fn list_collections(State(api): Shared) -> Reply {
    with_registry(&api, |r| {
        let list: Vec<CollectionSummary> = r
            .list()
            .into_iter()
            .map(|(name, c)| CollectionSummary {
                name,
                points: c.len(),
                vectors: c.config(),
                segments: c.segment_count(),
                failure: c.failure(),
            })
            .collect();
        Ok(json!({ "collections": list }))
    })
    .await
}

async fn create_collection(
    State(api): Shared,
    Path(name): Path<String>,
    Body(body): Body<CreateCollection>,
) -> Reply {
    with_registry(&api, move |r| {
        let created = r.create(&name, body.vectors)?;
        Ok(json!({ "created": created }))
    })
    .await
}

async fn describe_collection(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_collection(&api, name.clone(), move |c| {
        Ok(CollectionInfo {
            name,
            vectors: c.config(),
            points: c.len(),
            segments: c.segment_count(),
            indexes: c.payload_indexes(),
        })
    })
    .await
}

async fn collection_status(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_collection(&api, name, |c| {
        let status = c.status();
        let wal = c.wal_stats();
        let optimizer = c.optimizer_status();
        let options = c.optimizer_options();
        let graph = status.options.graph;
        let segments: Vec<Value> = status
            .segments
            .iter()
            .map(|s| {
                json!({
                    "id": s.id,
                    "rows": s.rows,
                    "live": s.live,
                    "index": if s.graph_nodes.is_some() { "hnsw" } else { "plain" },
                    "graph_bytes": s.graph_bytes,
                    "vector_bytes": s.vector_bytes,
                    "disk_bytes": s.disk_bytes,
                })
            })
            .collect();
        let segment_disk: u64 = status.segments.iter().map(|s| s.disk_bytes).sum();
        let job = |j: &Job| match j {
            Job::Flush => json!({"kind": "flush"}),
            Job::Merge { inputs, output } => {
                json!({"kind": "merge", "inputs": inputs, "output": output})
            }
            Job::Vacuum { input, output } => {
                json!({"kind": "vacuum", "inputs": [input], "output": output})
            }
        };
        Ok(json!({
            "points": c.len(),
            "failure": c.failure(),
            "segments": segments,
            "memtable": {
                "entries": status.memtable_entries,
                "bytes": status.memtable_bytes,
                "flush_bytes": status.options.flush_bytes,
                "frozen_entries": status.frozen_entries,
            },
            "wal": {
                "bytes": wal.disk_bytes,
                "last_seq": wal.last_seq,
                "fsyncs": wal.syncs,
            },
            "index": {
                "m": graph.config.m,
                "ef_construct": graph.config.ef_construct,
                "min_bytes": graph.min_bytes,
                "default_ef": nocturnum_query::collection::DEFAULT_HNSW_EF,
            },
            "optimizer": {
                "enabled": options.is_some(),
                "max_segments": options.map(|o| o.max_segments),
                "vacuum_ratio": options.map(|o| o.vacuum_ratio),
                "running": optimizer.running,
                "progress": optimizer.progress.map(|p| json!({
                    "entries": p.entries,
                    "entries_done": p.entries_done,
                    "graph_rows": p.graph_rows,
                    "graph_done": p.graph_done,
                })),
                "last": optimizer.last.as_ref().map(job),
                "last_error": optimizer.last_error,
            },
            "disk_bytes": segment_disk + wal.disk_bytes,
        }))
    })
    .await
}

async fn delete_collection(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_registry(&api, move |r| {
        if r.delete(&name)? {
            Ok(json!({"deleted": true}))
        } else {
            Err(ApiError::not_found(format!("no collection named {name:?}")))
        }
    })
    .await
}

// Writes.

async fn upsert(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<Upsert>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let VectorConfig { dim, distance } = c.config();
        let mut points = Vec::with_capacity(body.points.len());
        for (i, p) in body.points.into_iter().enumerate() {
            distance
                .check(dim, &p.vector)
                .map_err(|e| ApiError::from(e).at(format!("points[{i}].vector")))?;
            p.payload
                .validate()
                .map_err(|e| ApiError::from(e).at(format!("points[{i}].payload")))?;
            let sparse = match p.sparse {
                None => None,
                Some(v) => v
                    .prepare()
                    .map_err(|e| ApiError::from(e).at(format!("points[{i}].sparse")))?,
            };
            points.push(PointWrite {
                id: p.id,
                vector: p.vector,
                sparse,
                payload: p.payload,
            });
        }
        let seq = c
            .upsert(points, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

async fn delete_points(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<Select>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let selector = body.selector()?;
        let seq = c
            .delete(&selector, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

async fn set_payload(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<SetPayload>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let (selector, payload) = body.split()?;
        payload
            .validate()
            .map_err(|e| ApiError::from(e).at("payload"))?;
        let seq = c
            .set_payload(&selector, payload, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

async fn overwrite_payload(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<SetPayload>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let (selector, payload) = body.split()?;
        payload
            .validate()
            .map_err(|e| ApiError::from(e).at("payload"))?;
        let seq = c
            .overwrite_payload(&selector, payload, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

async fn delete_keys(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<DeleteKeys>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let (selector, keys) = body.split()?;
        let seq = c
            .delete_payload_keys(&selector, keys, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

async fn clear_payload(
    State(api): Shared,
    Path(name): Path<String>,
    Params(p): Params<AckParams>,
    Body(body): Body<Select>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let selector = body.selector()?;
        let seq = c
            .clear_payload(&selector, p.ack)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(WriteResult { seq, ack: p.ack })
    })
    .await
}

// Reads.

async fn recall(State(api): Shared, Path(name): Path<String>, Body(body): Body<Recall>) -> Reply {
    with_collection(&api, name, move |c| {
        let req = RecallRequest {
            samples: body.samples,
            limit: body.limit,
            hnsw_ef: body.widths(),
            filter: body.filter,
            seed: body.seed,
        };
        let r = c.recall(&req).map_err(|e| ApiError::from_query(c, e))?;
        let ms = |d: std::time::Duration| (d.as_secs_f64() * 1e6).round() / 1e3;
        Ok(json!({
            "samples": r.samples,
            "limit": r.limit,
            "points": r.points,
            "indexed_points": r.indexed_points,
            "exact": {"p50_ms": ms(r.exact_p50), "p99_ms": ms(r.exact_p99)},
            "runs": r.runs.iter().map(|x| json!({
                "hnsw_ef": x.hnsw_ef,
                "recall": x.recall,
                "p50_ms": ms(x.p50),
                "p99_ms": ms(x.p99),
            })).collect::<Vec<_>>(),
        }))
    })
    .await
}

async fn get_point(
    State(api): Shared,
    Path((name, id)): Path<(String, String)>,
    Params(p): Params<GetParams>,
) -> Reply {
    let id: PointId = id
        .parse()
        .map_err(|e: nocturnum_core::id::ParsePointIdError| ApiError::invalid(e.to_string()))?;
    with_collection(&api, name, move |c| {
        let with_payload = if p.with_payload {
            WithPayload::All
        } else {
            WithPayload::None
        };
        c.retrieve(&[id], &with_payload, p.with_vector)
            .map_err(|e| ApiError::from_query(c, e))?
            .pop()
            .map(PointOut::from)
            .ok_or_else(|| ApiError::not_found(format!("no point with id {id}")))
    })
    .await
}

async fn get_points(
    State(api): Shared,
    Path(name): Path<String>,
    Body(body): Body<GetPoints>,
) -> Reply {
    if body.ids.len() > MAX_RESULT_WINDOW {
        return Err(ApiError::invalid(format!(
            "at most {MAX_RESULT_WINDOW} ids can be read at once, got {}",
            body.ids.len()
        ))
        .at("ids"));
    }
    with_collection(&api, name, move |c| {
        let points = c
            .retrieve(&body.ids, &body.with_payload.0, body.with_vector)
            .map_err(|e| query_error(c, e, "text"))?;
        Ok(Points {
            points: points.into_iter().map(PointOut::from).collect(),
        })
    })
    .await
}

async fn scroll(State(api): Shared, Path(name): Path<String>, Body(body): Body<Scroll>) -> Reply {
    with_collection(&api, name, move |c| {
        let page = c
            .scroll(&ScrollRequest {
                filter: body.filter,
                after: body.after,
                limit: body.limit,
                with_payload: body.with_payload.0,
                with_vector: body.with_vector,
            })
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(Page {
            points: page.points.into_iter().map(PointOut::from).collect(),
            next: page.next,
        })
    })
    .await
}

async fn count(State(api): Shared, Path(name): Path<String>, Body(body): Body<Count>) -> Reply {
    with_collection(&api, name, move |c| {
        let n = c
            .count(body.filter.as_ref())
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(json!({ "count": n }))
    })
    .await
}

async fn search(State(api): Shared, Path(name): Path<String>, Body(body): Body<Search>) -> Reply {
    with_collection(&api, name, move |c| {
        let query = leaf_query(c, body.vector, body.text, body.sparse)?;
        let points = c
            .search(&SearchRequest {
                query,
                filter: body.filter,
                limit: body.limit,
                offset: body.offset,
                score_threshold: body.score_threshold,
                exact: body.exact,
                hnsw_ef: body.hnsw_ef,
                with_payload: body.with_payload.0,
                with_vector: body.with_vector,
            })
            .map_err(|e| query_error(c, e, "text"))?;
        Ok(Points {
            points: points.into_iter().map(ScoredOut::from).collect(),
        })
    })
    .await
}

async fn query(State(api): Shared, Path(name): Path<String>, Body(body): Body<QueryIn>) -> Reply {
    with_collection(&api, name, move |c| {
        let fusion = match (body.fusion, body.k) {
            (FusionIn::Rrf, k) => Fusion::Rrf {
                k: k.unwrap_or(DEFAULT_RRF_K),
            },
            (FusionIn::Dbsf, None) => Fusion::Dbsf,
            (FusionIn::Dbsf, Some(_)) => {
                return Err(ApiError::invalid("k only applies to rrf fusion").at("k"));
            }
        };
        if let Err(e) = fusion.validate() {
            return Err(ApiError::invalid(e).at("k"));
        }
        let mut prefetch = Vec::with_capacity(body.prefetch.len());
        for (i, p) in body.prefetch.into_iter().enumerate() {
            let at = format!("prefetch[{i}]");
            let query = leaf_query(c, p.vector, p.text, p.sparse).map_err(|e| e.within(&at))?;
            prefetch.push(Prefetch {
                query,
                filter: p.filter,
                limit: p.limit,
                score_threshold: p.score_threshold,
                exact: p.exact,
                hnsw_ef: p.hnsw_ef,
                weight: p.weight.unwrap_or(1.0),
            });
        }
        let result = c
            .query(&FusedRequest {
                prefetch,
                fusion,
                filter: body.filter,
                limit: body.limit,
                offset: body.offset,
                with_payload: body.with_payload.0,
                with_vector: body.with_vector,
            })
            .map_err(|e| query_error(c, e, "text"))?;
        Ok(QueryOut {
            points: result.points.into_iter().map(ScoredOut::from).collect(),
            candidates: CandidatesOut {
                pool: result.candidates.pool,
                sources: result
                    .candidates
                    .sources
                    .into_iter()
                    .map(|(limit, found)| SourceOut { limit, found })
                    .collect(),
            },
        })
    })
    .await
}

/// What a search ranks by, from exactly one of its `vector`, `text` and `sparse`.
fn leaf_query(
    c: &Collection,
    vector: Option<Vec<f32>>,
    text: Option<TextIn>,
    sparse: Option<SparseIn>,
) -> Result<Query, ApiError> {
    match (vector, text, sparse) {
        (Some(vector), None, None) => {
            let VectorConfig { dim, distance } = c.config();
            distance
                .check(dim, &vector)
                .map_err(|e| ApiError::from(e).at("vector"))?;
            Ok(Query::Vector(vector))
        }
        (None, Some(text), None) => Ok(Query::Text(text_query(text, "text")?)),
        (None, None, Some(s)) => {
            let vector = SparseVector {
                indices: s.indices,
                values: s.values,
            };
            // Checked here so the error points at it, the engine prepares it again.
            vector
                .clone()
                .prepare()
                .map_err(|e| ApiError::from(e).at("sparse"))?;
            Ok(Query::Sparse(SparseQuery { vector, idf: s.idf }))
        }
        _ => Err(ApiError::invalid(
            "a search needs exactly one of vector, text and sparse",
        )),
    }
}

/// Maps a search error, pointing text-query errors inside `at`, and inside their prefetch
/// for a hybrid query.
fn query_error(c: &Collection, e: QueryError, at: &str) -> ApiError {
    let field = match e {
        QueryError::Prefetch(i, inner) => {
            return query_error(c, *inner, at).within(&format!("prefetch[{i}]"));
        }
        QueryError::NoTextIndex(_) => Some("field"),
        QueryError::TooManyWords => Some("query"),
        _ => None,
    };
    let error = ApiError::from_query(c, e);
    match field {
        Some(f) => error.at(format!("{at}.{f}")),
        None => error,
    }
}

/// A text search from its request body, checking the BM25 knobs where `at` names them.
fn text_query(text: TextIn, at: &str) -> Result<TextQuery, ApiError> {
    let default = Bm25::default();
    let bm25 = Bm25 {
        k1: text.k1.unwrap_or(default.k1),
        b: text.b.unwrap_or(default.b),
    };
    if let Err(e) = bm25.validate() {
        let knob = if e.starts_with("k1") { "k1" } else { "b" };
        return Err(ApiError::invalid(e).at(format!("{at}.{knob}")));
    }
    Ok(TextQuery {
        field: text.field,
        query: text.query,
        bm25,
    })
}

// Payload indexes.

async fn list_indexes(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_collection(&api, name, |c| Ok(json!({"indexes": c.payload_indexes()}))).await
}

async fn create_index(
    State(api): Shared,
    Path(name): Path<String>,
    Body(spec): Body<PayloadIndexSpec>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let created = c
            .create_payload_index(spec)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(json!({ "created": created }))
    })
    .await
}

async fn delete_index(
    State(api): Shared,
    Path(name): Path<String>,
    Body(spec): Body<PayloadIndexSpec>,
) -> Reply {
    with_collection(&api, name, move |c| {
        let deleted = c
            .drop_payload_index(&spec)
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(json!({ "deleted": deleted }))
    })
    .await
}

// Snapshots.

async fn take_snapshot(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_registry(&api, move |r| r.take_snapshot(&name)).await
}

async fn list_snapshots(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_registry(&api, move |r| Ok(json!({"snapshots": r.snapshots(&name)?}))).await
}

async fn download_snapshot(
    State(api): Shared,
    Path((name, snapshot)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let file = snapshot.clone();
    let path = on_registry(&api, move |r| r.snapshot_path(&name, &file)).await?;
    let f = tokio::fs::File::open(&path)
        .await
        .map_err(|e| ApiError::internal(format!("opening {}: {e}", path.display())))?;
    let len = f
        .metadata()
        .await
        .map_err(|e| ApiError::internal(format!("reading {}: {e}", path.display())))?
        .len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/x-tar".to_owned()),
            (header::CONTENT_LENGTH, len.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{snapshot}\""),
            ),
        ],
        HttpBody::from_stream(ReaderStream::new(f)),
    )
        .into_response())
}

async fn delete_snapshot(
    State(api): Shared,
    Path((name, snapshot)): Path<(String, String)>,
) -> Reply {
    with_registry(&api, move |r| {
        r.delete_snapshot(&name, &snapshot)?;
        Ok(json!({"deleted": true}))
    })
    .await
}

async fn get_schedule(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_registry(&api, move |r| r.schedule(&name)).await
}

async fn set_schedule(
    State(api): Shared,
    Path(name): Path<String>,
    Body(schedule): Body<crate::schedule::Schedule>,
) -> Reply {
    with_registry(&api, move |r| r.set_schedule(&name, schedule)).await
}

async fn delete_schedule(State(api): Shared, Path(name): Path<String>) -> Reply {
    with_registry(&api, move |r| {
        Ok(json!({"deleted": r.delete_schedule(&name)?}))
    })
    .await
}

async fn restore(State(api): Shared, Path(name): Path<String>, Body(body): Body<Restore>) -> Reply {
    with_registry(&api, move |r| {
        r.restore(&name, &body.snapshot)?;
        Ok(json!({"restored": true}))
    })
    .await
}
