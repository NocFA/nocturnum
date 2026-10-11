//! A collection: the operation layer every transport wraps.
//!
//! A [`Collection`] owns one shard (v1 is single node and single shard) and turns requests into
//! shard writes and reads: writes go through the WAL with the caller's acknowledgement level,
//! and reads take a consistent [`View`] of the memtable and the segments.
//!
//! # Reads
//!
//! - **Search** scores the memtable's points directly, and for each segment walks its graph
//!   index or scans its candidate rows, as the planner chooses (see [`crate::plan`]), then
//!   merges the per-segment results into one top `offset + limit`, ordered by score and then
//!   by id. `exact` skips the graphs.
//! - **Scroll** walks matching points in id order, resuming after the last id of the
//!   previous page, so pages stay stable while points before the cursor change.
//! - **Count** is exact, settling rows the payload indexes cannot by reading their payloads.
//!
//! Filters on segments go through their payload indexes, see
//! `docs/decisions/0003-payload-indexes.md`, and filters on the memtable through the reference
//! evaluator, [`Filter::matches`].
//!
//! # Writes by filter
//!
//! Deleting or changing payloads by filter resolves the filter to ids and logs an operation
//! naming those ids, both under the shard's write lock, so no other write lands in between and
//! replaying the WAL repeats exactly what happened.

use std::collections::{BinaryHeap, HashSet};
use std::path::Path;

use nocturnum_core::payload::PathStep;
use nocturnum_core::{Ack, Filter, JsonPath, Payload, PayloadIndexSpec, PointId, VectorConfig};
use nocturnum_index::flat::search_filtered;

use crate::plan::{PlanOptions, Strategy, choose};
use nocturnum_index::{Bounds, TopK};
use nocturnum_storage::{
    Operation, PointRef, PointWrite, SegmentView, Shard, ShardOptions, ShardRecovery, View,
    WriteError,
};
use serde_json::{Map, Value};

/// The most results one search can reach, counting `offset`, and the most points one scroll
/// page holds, which bounds the memory a single request can take.
pub const MAX_RESULT_WINDOW: usize = 10_000;

/// The graph search beam width when a request gives none.
pub const DEFAULT_HNSW_EF: usize = 128;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The request is malformed or breaks a limit, and nothing was done.
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Storage(#[from] nocturnum_storage::Error),
}

impl From<nocturnum_core::Error> for QueryError {
    fn from(e: nocturnum_core::Error) -> Self {
        QueryError::Invalid(e.to_string())
    }
}

impl From<WriteError> for QueryError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Invalid(e) => e.into(),
            WriteError::Storage(e) => e.into(),
        }
    }
}

pub type Result<T, E = QueryError> = std::result::Result<T, E>;

/// Which points a write applies to.
#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    Ids(Vec<PointId>),
    Filter(Filter),
}

/// How much of each payload a read returns.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum WithPayload {
    #[default]
    None,
    All,
    /// Only these paths, which must be plain object keys such as `a` or `a.b`.
    Include(Vec<JsonPath>),
    /// Everything except these paths, which must be plain object keys.
    Exclude(Vec<JsonPath>),
}

/// A point as a read returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    pub id: PointId,
    pub payload: Option<Payload>,
    pub vector: Option<Vec<f32>>,
}

/// A search result.
#[derive(Debug, Clone, PartialEq)]
pub struct ScoredPoint {
    pub id: PointId,
    /// The metric's own value: a similarity for cosine and dot, a distance for euclid and
    /// manhattan.
    pub score: f32,
    pub payload: Option<Payload>,
    pub vector: Option<Vec<f32>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub vector: Vec<f32>,
    pub filter: Option<Filter>,
    pub limit: usize,
    /// Results to skip before the first one returned.
    pub offset: usize,
    /// Drops results below this similarity, or above this distance for euclid and manhattan.
    pub score_threshold: Option<f32>,
    /// Scans every candidate instead of walking segments' graphs, exact but slower.
    pub exact: bool,
    /// The beam width for graph search, higher finds more of the true nearest points more
    /// slowly, [`DEFAULT_HNSW_EF`] when not given and never below `offset + limit`.
    pub hnsw_ef: Option<usize>,
    pub with_payload: WithPayload,
    pub with_vector: bool,
}

impl SearchRequest {
    /// A plain search for the `limit` nearest points.
    pub fn new(vector: Vec<f32>, limit: usize) -> SearchRequest {
        SearchRequest {
            vector,
            filter: None,
            limit,
            offset: 0,
            score_threshold: None,
            exact: false,
            hnsw_ef: None,
            with_payload: WithPayload::None,
            with_vector: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScrollRequest {
    pub filter: Option<Filter>,
    /// Starts after this id, `None` for the first page.
    pub after: Option<PointId>,
    pub limit: usize,
    pub with_payload: WithPayload,
    pub with_vector: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ScrollPage {
    pub points: Vec<Point>,
    /// The cursor for the next page, `None` on the last one.
    pub next: Option<PointId>,
}

pub struct Collection {
    shard: Shard,
    plan: PlanOptions,
}

impl Collection {
    /// Opens the collection in `dir`, creating it with `create` when it does not exist yet.
    pub fn open(
        dir: impl AsRef<Path>,
        options: ShardOptions,
        create: Option<VectorConfig>,
    ) -> Result<(Collection, ShardRecovery)> {
        let (shard, recovery) = Shard::open(dir, options, create)?;
        let plan = PlanOptions::for_dim(shard.config().dim);
        Ok((Collection { shard, plan }, recovery))
    }

    /// Replaces the planner's thresholds, which default to [`PlanOptions::for_dim`].
    pub fn with_plan_options(mut self, plan: PlanOptions) -> Collection {
        self.plan = plan;
        self
    }

    pub fn config(&self) -> VectorConfig {
        self.shard.config()
    }

    /// The number of points.
    pub fn len(&self) -> usize {
        self.shard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.shard.is_empty()
    }

    /// Writes the memtable out as a segment, see [`Shard::flush`].
    pub fn flush(&self) -> Result<bool> {
        Ok(self.shard.flush()?)
    }

    pub fn segment_count(&self) -> usize {
        self.shard.segment_ids().len()
    }

    pub fn upsert(&self, points: Vec<PointWrite>, ack: Ack) -> Result<u64> {
        Ok(self.shard.write(Operation::Upsert(points), ack)?)
    }

    pub fn delete(&self, selector: &Selector, ack: Ack) -> Result<u64> {
        self.write_selected(selector, ack, Operation::Delete)
    }

    /// Sets the given top-level keys, leaving other keys as they are.
    pub fn set_payload(&self, selector: &Selector, payload: Payload, ack: Ack) -> Result<u64> {
        self.write_selected(selector, ack, |ids| Operation::SetPayload { ids, payload })
    }

    /// Replaces the whole payload.
    pub fn overwrite_payload(
        &self,
        selector: &Selector,
        payload: Payload,
        ack: Ack,
    ) -> Result<u64> {
        self.write_selected(selector, ack, |ids| Operation::OverwritePayload {
            ids,
            payload,
        })
    }

    pub fn delete_payload_keys(
        &self,
        selector: &Selector,
        keys: Vec<String>,
        ack: Ack,
    ) -> Result<u64> {
        self.write_selected(selector, ack, |ids| Operation::DeletePayloadKeys {
            ids,
            keys,
        })
    }

    pub fn clear_payload(&self, selector: &Selector, ack: Ack) -> Result<u64> {
        self.write_selected(selector, ack, Operation::ClearPayload)
    }

    pub fn payload_indexes(&self) -> Vec<PayloadIndexSpec> {
        self.shard.payload_indexes()
    }

    /// Adds a payload index and returns whether it was new.
    pub fn create_payload_index(&self, spec: PayloadIndexSpec) -> Result<bool> {
        Ok(self.shard.create_payload_index(spec)?)
    }

    /// Removes a payload index and returns whether it existed.
    pub fn drop_payload_index(&self, spec: &PayloadIndexSpec) -> Result<bool> {
        Ok(self.shard.drop_payload_index(spec)?)
    }

    /// The points with these ids, in the order asked for, skipping ids that do not exist and
    /// repeats.
    pub fn retrieve(
        &self,
        ids: &[PointId],
        with_payload: &WithPayload,
        with_vector: bool,
    ) -> Result<Vec<Point>> {
        check_projection(with_payload)?;
        let view = self.shard.view();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in ids {
            if !seen.insert(*id) {
                continue;
            }
            if let Some(rec) = view.get(*id)? {
                out.push(Point {
                    id: *id,
                    payload: project(&rec.payload, with_payload),
                    vector: with_vector.then_some(rec.vector),
                });
            }
        }
        Ok(out)
    }

    /// How many points match `filter`, or how many there are without one.
    pub fn count(&self, filter: Option<&Filter>) -> Result<u64> {
        let Some(filter) = filter else {
            return Ok(self.shard.len() as u64);
        };
        filter.validate()?;
        let view = self.shard.view();
        let mut n = 0;
        for p in view.recent() {
            if filter.matches(p.id, &*p.payload()?) {
                n += 1;
            }
        }
        for seg in view.segments() {
            let b = segment_bounds(&seg, Some(filter));
            n += b.sure.len();
            for row in b.uncertain() {
                if row_matches(&seg, filter, row)? {
                    n += 1;
                }
            }
        }
        Ok(n)
    }

    /// One page of matching points in id order.
    pub fn scroll(&self, req: &ScrollRequest) -> Result<ScrollPage> {
        if req.limit == 0 || req.limit > MAX_RESULT_WINDOW {
            return Err(QueryError::Invalid(format!(
                "limit must be between 1 and {MAX_RESULT_WINDOW}, got {}",
                req.limit
            )));
        }
        check_projection(&req.with_payload)?;
        if let Some(f) = &req.filter {
            f.validate()?;
        }
        let view = self.shard.view();
        // One more than the page, to know whether another page follows.
        let ids = smallest_matching(&view, req.filter.as_ref(), req.after, req.limit + 1)?;
        let more = ids.len() > req.limit;
        let mut points = Vec::with_capacity(req.limit);
        for id in ids.into_iter().take(req.limit) {
            let rec = view
                .get(id)?
                .expect("the view still holds every point it listed");
            points.push(Point {
                id,
                payload: project(&rec.payload, &req.with_payload),
                vector: req.with_vector.then_some(rec.vector),
            });
        }
        let next = if more {
            points.last().map(|p| p.id)
        } else {
            None
        };
        Ok(ScrollPage { points, next })
    }

    /// The best matches for a vector, best first.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<ScoredPoint>> {
        let window = req.offset.saturating_add(req.limit);
        if window > MAX_RESULT_WINDOW {
            return Err(QueryError::Invalid(format!(
                "offset plus limit may be at most {MAX_RESULT_WINDOW}, got {window}"
            )));
        }
        check_projection(&req.with_payload)?;
        if let Some(f) = &req.filter {
            f.validate()?;
        }
        let VectorConfig { dim, distance } = self.shard.config();
        let query = distance.prepare(dim, req.vector.clone())?;
        if window == 0 {
            return Ok(Vec::new());
        }

        let view = self.shard.view();
        let mut top = TopK::new(window);
        for p in view.recent() {
            let score = distance.score(&query, p.vector);
            if top.would_keep(&p.id, score)
                && req
                    .filter
                    .as_ref()
                    .map_or(Ok(true), |f| p.payload().map(|pl| f.matches(p.id, &pl)))?
            {
                top.push(p.id, score);
            }
        }
        let ef = req.hnsw_ef.unwrap_or(DEFAULT_HNSW_EF).max(window);
        for seg in view.segments() {
            let bounds = segment_bounds(&seg, req.filter.as_ref());
            let check = |row| match &req.filter {
                Some(f) => row_matches(&seg, f, row),
                None => Ok(true),
            };
            let vectors = seg.vectors();
            let graph = seg.graph().filter(|_| !req.exact);
            // Rows are in id order within a segment, so ties cut inside it keep the same ids
            // the merge would.
            let hits = match graph {
                None => search_filtered(distance, vectors, dim, &query, window, &bounds, check)?,
                Some(g) => {
                    let plan = choose(bounds, &self.plan, check)?;
                    let b = &plan.bounds;
                    match plan.strategy {
                        Strategy::Enumerate => {
                            search_filtered(distance, vectors, dim, &query, window, b, check)?
                        }
                        Strategy::Graph => g.search(vectors, &query, window, ef, |row| {
                            if b.sure.contains(row) {
                                Ok(true)
                            } else if b.maybe.contains(row) {
                                check(row)
                            } else {
                                Ok(false)
                            }
                        })?,
                    }
                }
            };
            for h in hits {
                top.push(seg.point(h.key).id, h.score);
            }
        }

        let mut out = Vec::new();
        for s in top.into_sorted_vec().into_iter().skip(req.offset) {
            let score = distance.to_user(s.score);
            if let Some(t) = req.score_threshold {
                let passes = if distance.user_higher_is_better() {
                    score >= t
                } else {
                    score <= t
                };
                // Results come best first, so the rest fail too.
                if !passes {
                    break;
                }
            }
            let (payload, vector) = if req.with_payload == WithPayload::None && !req.with_vector {
                (None, None)
            } else {
                let rec = view
                    .get(s.key)?
                    .expect("the view still holds every point it scored");
                (
                    project(&rec.payload, &req.with_payload),
                    req.with_vector.then_some(rec.vector),
                )
            };
            out.push(ScoredPoint {
                id: s.key,
                score,
                payload,
                vector,
            });
        }
        Ok(out)
    }

    /// Logs the operation `make` builds for the selected ids, resolving a filter under the
    /// shard's write lock so the ids are exactly the matching points at that moment.
    fn write_selected(
        &self,
        selector: &Selector,
        ack: Ack,
        make: impl FnOnce(Vec<PointId>) -> Operation,
    ) -> Result<u64> {
        match selector {
            Selector::Ids(ids) => Ok(self.shard.write(make(ids.clone()), ack)?),
            Selector::Filter(f) => {
                f.validate()?;
                Ok(self.shard.write_with(ack, |view| {
                    Ok(make(smallest_matching(view, Some(f), None, usize::MAX)?))
                })?)
            }
        }
    }
}

/// The rows of a segment `filter` surely and maybe matches, among its live rows.
fn segment_bounds(seg: &SegmentView<'_>, filter: Option<&Filter>) -> Bounds {
    match filter {
        Some(f) => {
            let mut b = seg.payload_index().bounds(f);
            b.restrict(seg.live());
            b
        }
        None => Bounds {
            sure: seg.live().clone(),
            maybe: seg.live().clone(),
        },
    }
}

fn row_matches(
    seg: &SegmentView<'_>,
    filter: &Filter,
    row: u32,
) -> nocturnum_storage::Result<bool> {
    let p = seg.point(row);
    Ok(filter.matches(p.id, &*p.payload()?))
}

fn point_matches(p: &PointRef<'_>, filter: Option<&Filter>) -> nocturnum_storage::Result<bool> {
    match filter {
        Some(f) => Ok(f.matches(p.id, &*p.payload()?)),
        None => Ok(true),
    }
}

/// The `n` smallest ids above `after` that match `filter`, in order.
fn smallest_matching(
    view: &View<'_>,
    filter: Option<&Filter>,
    after: Option<PointId>,
    n: usize,
) -> nocturnum_storage::Result<Vec<PointId>> {
    // A max-heap of the smallest ids seen, so its top is the one to beat.
    let mut heap: BinaryHeap<PointId> = BinaryHeap::new();
    let offer = |id: PointId, heap: &mut BinaryHeap<PointId>| {
        heap.push(id);
        if heap.len() > n {
            heap.pop();
        }
    };
    let wanted = |id: PointId, heap: &BinaryHeap<PointId>| {
        after.is_none_or(|a| id > a) && (heap.len() < n || heap.peek().is_some_and(|top| id < *top))
    };
    for p in view.recent() {
        if wanted(p.id, &heap) && point_matches(&p, filter)? {
            offer(p.id, &mut heap);
        }
    }
    for seg in view.segments() {
        let b = segment_bounds(&seg, filter);
        for row in &b.maybe {
            let p = seg.point(row);
            // The id check comes first, so rows that cannot make the page are never read.
            if wanted(p.id, &heap) && (b.sure.contains(row) || point_matches(&p, filter)?) {
                offer(p.id, &mut heap);
            }
        }
    }
    Ok(heap.into_sorted_vec())
}

/// Checks include and exclude paths are plain object keys.
fn check_projection(with: &WithPayload) -> Result<()> {
    if let WithPayload::Include(paths) | WithPayload::Exclude(paths) = with {
        for p in paths {
            if !p.steps().iter().all(|s| matches!(s, PathStep::Key(_))) {
                return Err(QueryError::Invalid(format!(
                    "payload paths to include or exclude must be object keys, {p} is not"
                )));
            }
        }
    }
    Ok(())
}

fn keys(path: &JsonPath) -> impl Iterator<Item = &str> {
    path.steps().iter().map(|s| match s {
        PathStep::Key(k) => k.as_str(),
        _ => unreachable!("checked by check_projection"),
    })
}

/// The part of `payload` that `with` asks for.
fn project(payload: &Payload, with: &WithPayload) -> Option<Payload> {
    match with {
        WithPayload::None => None,
        WithPayload::All => Some(payload.clone()),
        WithPayload::Include(paths) => {
            let mut out = Map::new();
            for path in paths {
                let path: Vec<&str> = keys(path).collect();
                if let Some(v) = lookup(&payload.0, &path) {
                    insert(&mut out, &path, v.clone());
                }
            }
            Some(Payload(out))
        }
        WithPayload::Exclude(paths) => {
            let mut out = payload.0.clone();
            for path in paths {
                let path: Vec<&str> = keys(path).collect();
                remove(&mut out, &path);
            }
            Some(Payload(out))
        }
    }
}

fn lookup<'a>(map: &'a Map<String, Value>, path: &[&str]) -> Option<&'a Value> {
    let (first, rest) = path.split_first()?;
    let v = map.get(*first)?;
    match (rest.is_empty(), v) {
        (true, _) => Some(v),
        (false, Value::Object(inner)) => lookup(inner, rest),
        _ => None,
    }
}

fn insert(map: &mut Map<String, Value>, path: &[&str], value: Value) {
    let (first, rest) = path.split_first().expect("paths have a key");
    if rest.is_empty() {
        map.insert((*first).to_owned(), value);
        return;
    }
    let entry = map
        .entry((*first).to_owned())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(inner) = entry {
        insert(inner, rest, value);
    }
}

fn remove(map: &mut Map<String, Value>, path: &[&str]) {
    let Some((first, rest)) = path.split_first() else {
        return;
    };
    if rest.is_empty() {
        map.remove(*first);
    } else if let Some(Value::Object(inner)) = map.get_mut(*first) {
        remove(inner, rest);
    }
}

#[cfg(test)]
mod tests;
