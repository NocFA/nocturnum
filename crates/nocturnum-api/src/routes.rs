//! The endpoints, each a thin call into a [`Collection`] or the [`Registry`].

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Body as HttpBody;
use axum::extract::{Extension, Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use nocturnum_core::{PayloadIndexSpec, PointId, VectorConfig};
use nocturnum_query::collection::MAX_RESULT_WINDOW;
use nocturnum_query::{Collection, RecallRequest, ScrollRequest, SearchRequest, WithPayload};
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
            points.push(PointWrite {
                id: p.id,
                vector: p.vector,
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
            .map_err(|e| ApiError::from_query(c, e))?;
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
        let VectorConfig { dim, distance } = c.config();
        distance
            .check(dim, &body.vector)
            .map_err(|e| ApiError::from(e).at("vector"))?;
        let points = c
            .search(&SearchRequest {
                vector: body.vector,
                filter: body.filter,
                limit: body.limit,
                offset: body.offset,
                score_threshold: body.score_threshold,
                exact: body.exact,
                hnsw_ef: body.hnsw_ef,
                with_payload: body.with_payload.0,
                with_vector: body.with_vector,
            })
            .map_err(|e| ApiError::from_query(c, e))?;
        Ok(Points {
            points: points.into_iter().map(ScoredOut::from).collect(),
        })
    })
    .await
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
