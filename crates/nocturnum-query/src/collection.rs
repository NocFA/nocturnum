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
use std::sync::Arc;

use nocturnum_core::payload::PathStep;
use nocturnum_core::{
    Ack, Condition, Distance, Filter, JsonPath, Payload, PayloadIndexSpec, PointId, SparseVector,
    VectorConfig,
};
use nocturnum_index::flat::search_filtered_until;

pub use recall::{
    MAX_RECALL_EF_VALUES, MAX_RECALL_LIMIT, MAX_RECALL_SAMPLES, RECALL_TIME_BUDGET, RecallReport,
    RecallRequest, RecallRun,
};

use crate::deadline::Deadline;
use crate::fusion::{Fusion, check_weight};
use crate::plan::{PlanOptions, Strategy, choose};
use nocturnum_index::{Bm25, Bounds, Scored, TopK};
use nocturnum_storage::{
    Operation, Optimizer, OptimizerOptions, OptimizerStatus, PointRef, PointWrite, SegmentView,
    Shard, ShardOptions, ShardRecovery, ShardStatus, SnapshotInfo, View, WalStats, WriteError,
};
use serde_json::{Map, Value};

/// The most results one search can reach, counting `offset`, and the most points one scroll
/// page holds, which bounds the memory a single request can take.
pub const MAX_RESULT_WINDOW: usize = 10_000;

/// The graph search beam width when a request gives none.
pub const DEFAULT_HNSW_EF: usize = 128;

/// The widest graph search beam a request may ask for, since the beam's memory is taken up
/// front.
pub const MAX_HNSW_EF: usize = 65_536;

#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// The request is malformed or breaks a limit, and nothing was done.
    #[error("invalid request: {0}")]
    Invalid(String),
    /// A vector, payload, filter or path in the request failed validation, and nothing was
    /// done.
    #[error(transparent)]
    Input(#[from] nocturnum_core::Error),
    #[error(transparent)]
    Storage(#[from] nocturnum_storage::Error),
    /// One field of the request is out of range, `at` naming it, such as `limit`.
    #[error("{message}")]
    Field { at: String, message: String },
    /// A text search named a field without a text index.
    #[error("{0} has no text index, create one with kind text to search it")]
    NoTextIndex(JsonPath),
    /// A text query has more than [`MAX_QUERY_TERMS`] different words.
    #[error("a text query may have at most {MAX_QUERY_TERMS} different words")]
    TooManyWords,
    /// One of a fused query's prefetches failed, the first number being its place in the list.
    #[error("prefetch {0}: {1}")]
    Prefetch(usize, Box<QueryError>),
    /// The work ran out of the time it is allowed, and nothing was changed.
    #[error("{0}")]
    TimedOut(String),
}

impl QueryError {
    /// Whether the request was refused for what it asked, rather than failing in storage.
    pub fn is_invalid(&self) -> bool {
        matches!(
            self,
            QueryError::Invalid(_)
                | QueryError::Field { .. }
                | QueryError::Input(_)
                | QueryError::NoTextIndex(_)
                | QueryError::TooManyWords
        ) || matches!(self, QueryError::Prefetch(_, e) if e.is_invalid())
    }
}

impl QueryError {
    fn field(at: &str, message: impl Into<String>) -> QueryError {
        QueryError::Field {
            at: at.to_owned(),
            message: message.into(),
        }
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
    /// With the vector, when the point has a sparse vector.
    pub sparse: Option<SparseVector>,
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
    /// With the vector, when the point has a sparse vector.
    pub sparse: Option<SparseVector>,
}

/// What a search ranks points by.
#[derive(Debug, Clone, PartialEq)]
pub enum Query {
    /// Nearness to a dense vector, by the collection's distance.
    Vector(Vec<f32>),
    /// BM25 relevance to some words, over a field with a text index.
    Text(TextQuery),
    /// The dot product with a sparse vector, over the points that have one.
    Sparse(SparseQuery),
}

/// A text search, see `nocturnum_index::text` for how it scores.
#[derive(Debug, Clone, PartialEq)]
pub struct TextQuery {
    /// A field with a text index.
    pub field: JsonPath,
    /// The words to look for.
    pub query: String,
    pub bm25: Bm25,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchRequest {
    pub query: Query,
    pub filter: Option<Filter>,
    pub limit: usize,
    /// Results to skip before the first one returned.
    pub offset: usize,
    /// Drops results below this similarity or text score, or above this distance for euclid
    /// and manhattan.
    pub score_threshold: Option<f32>,
    /// Scans every candidate instead of walking segments' graphs, exact but slower. Text
    /// search is always exact.
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
        SearchRequest::of(Query::Vector(vector), limit)
    }

    /// A plain search for the `limit` best points by `query`.
    pub fn of(query: Query, limit: usize) -> SearchRequest {
        SearchRequest {
            query,
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

/// The most prefetches one fused query may run.
pub const MAX_PREFETCH: usize = 16;

/// One search feeding a fused query.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefetch {
    pub query: Query,
    /// Applied together with the fused query's own filter.
    pub filter: Option<Filter>,
    /// How many of its best points it hands to fusion, the fused query's `offset + limit`
    /// when not given.
    pub limit: Option<usize>,
    pub score_threshold: Option<f32>,
    pub exact: bool,
    pub hnsw_ef: Option<usize>,
    /// How much its ranking counts in the fusion, from 0 up, 1 by default.
    pub weight: f64,
}

impl Prefetch {
    pub fn new(query: Query) -> Prefetch {
        Prefetch {
            query,
            filter: None,
            limit: None,
            score_threshold: None,
            exact: false,
            hnsw_ef: None,
            weight: 1.0,
        }
    }
}

/// A query fusing the results of several searches into one ranking, see [`crate::fusion`].
#[derive(Debug, Clone, PartialEq)]
pub struct FusedRequest {
    pub prefetch: Vec<Prefetch>,
    pub fusion: Fusion,
    /// Applied to every prefetch.
    pub filter: Option<Filter>,
    pub limit: usize,
    pub offset: usize,
    pub with_payload: WithPayload,
    pub with_vector: bool,
}

/// What a fused query had to choose from, so a client can see how far its prefetches' limits
/// let the ranking go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidates {
    /// The distinct points the prefetches handed to fusion, which is as far as the fused
    /// ranking goes.
    pub pool: usize,
    /// Each prefetch's limit and how many points it handed to fusion after its score
    /// threshold, in request order.
    pub sources: Vec<(usize, usize)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FusedResult {
    pub points: Vec<ScoredPoint>,
    pub candidates: Candidates,
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
    shard: Arc<Shard>,
    plan: PlanOptions,
    optimizer: Option<Optimizer>,
    optimizer_options: Option<OptimizerOptions>,
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
        let collection = Collection {
            shard: Arc::new(shard),
            plan,
            optimizer: None,
            optimizer_options: None,
        };
        Ok((collection, recovery))
    }

    /// Starts a background thread that flushes, merges and vacuums segments, see
    /// [`Shard::optimize_once`]. Dropping the collection stops it.
    pub fn start_optimizer(&mut self, options: OptimizerOptions) {
        self.optimizer = None;
        self.optimizer = Some(Optimizer::start(self.shard.clone(), options));
        self.optimizer_options = Some(options);
    }

    /// The options the background optimizer runs with, `None` when it is not running.
    pub fn optimizer_options(&self) -> Option<OptimizerOptions> {
        self.optimizer_options
    }

    /// The segments and the memtable as they are now, see [`Shard::status`].
    pub fn status(&self) -> ShardStatus {
        self.shard.status()
    }

    /// Writes a snapshot of the collection to `path`, with its checksum in `<path>.sha256`,
    /// while writes carry on. [`nocturnum_storage::shard::restore`] unpacks one into a new
    /// directory for [`Collection::open`].
    pub fn snapshot(&self, path: &Path) -> Result<SnapshotInfo> {
        Ok(self.shard.snapshot(path)?)
    }

    /// What the optimizer is doing and last did.
    pub fn optimizer_status(&self) -> OptimizerStatus {
        self.shard.optimizer_status()
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

    /// Why the collection has stopped taking writes, `None` while it is healthy, see
    /// [`Shard::failure`].
    pub fn failure(&self) -> Option<&'static str> {
        self.shard.failure()
    }

    pub fn wal_stats(&self) -> WalStats {
        self.shard.wal_stats()
    }

    /// Makes the WAL's next `n` fsyncs fail, for fault-injection tests.
    #[doc(hidden)]
    pub fn inject_sync_failures(&self, n: u64) {
        self.shard.inject_sync_failures(n);
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
                    sparse: rec.sparse.filter(|_| with_vector),
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
                sparse: rec.sparse.filter(|_| req.with_vector),
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

    /// The best matches for a vector or some words, best first.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<ScoredPoint>> {
        self.search_until(req, Deadline::NONE)
    }

    /// [`Collection::search`], stopping with [`QueryError::TimedOut`] once `deadline` passes.
    pub fn search_until(
        &self,
        req: &SearchRequest,
        deadline: Deadline,
    ) -> Result<Vec<ScoredPoint>> {
        self.search_in(&self.shard.view(), req, deadline)
    }

    /// [`Collection::search`] against a view the caller holds, so several searches can see
    /// the same points.
    fn search_in(
        &self,
        view: &View,
        req: &SearchRequest,
        deadline: Deadline,
    ) -> Result<Vec<ScoredPoint>> {
        let window = check_window(req.offset, req.limit)?;
        check_ef(&req.query, req.hnsw_ef)?;
        check_threshold(req.score_threshold)?;
        check_projection(&req.with_payload)?;
        if let Some(f) = &req.filter {
            f.validate()?;
        }
        let leaf = Leaf {
            query: &req.query,
            filter: req.filter.as_ref(),
            limit: window,
            exact: req.exact,
            hnsw_ef: req.hnsw_ef,
            score_threshold: req.score_threshold,
            deadline,
        };
        let (hits, scale) = self.rank(view, &leaf)?;
        let out = hits
            .into_iter()
            .skip(req.offset)
            .enumerate()
            .map(|(i, s)| {
                deadline.tick(i)?;
                let score = scale.to_user(s.score);
                scored(view, s.key, score, &req.with_payload, req.with_vector)
            })
            .collect::<Result<_>>()?;
        deadline.check()?;
        Ok(out)
    }

    /// Runs several searches and fuses their results into one ranking, best first.
    pub fn query(&self, req: &FusedRequest) -> Result<FusedResult> {
        self.query_until(req, Deadline::NONE)
    }

    /// [`Collection::query`], stopping with [`QueryError::TimedOut`] once `deadline` passes.
    pub fn query_until(&self, req: &FusedRequest, deadline: Deadline) -> Result<FusedResult> {
        let window = check_window(req.offset, req.limit)?;
        if !(1..=MAX_PREFETCH).contains(&req.prefetch.len()) {
            return Err(QueryError::field(
                "prefetch",
                format!(
                    "a query needs between 1 and {MAX_PREFETCH} prefetches, got {}",
                    req.prefetch.len()
                ),
            ));
        }
        req.fusion
            .validate()
            .map_err(|e| QueryError::field("k", e))?;
        check_projection(&req.with_payload)?;
        if let Some(f) = &req.filter {
            f.validate()?;
        }
        let mut limits = Vec::with_capacity(req.prefetch.len());
        for (i, p) in req.prefetch.iter().enumerate() {
            let limit = check_prefetch(p, req.filter.as_ref(), window)
                .map_err(|e| QueryError::Prefetch(i, e.into()))?;
            limits.push(limit);
        }
        let budget: usize = limits.iter().sum();
        if budget < window {
            return Err(QueryError::field(
                "limit",
                format!(
                    "offset plus limit is {window}, more than the {budget} points the \
                     prefetches can return together, so raise their limits"
                ),
            ));
        }

        // Every prefetch reads the same view, so they all rank the same points.
        let view = self.shard.view();
        let mut lists = Vec::with_capacity(req.prefetch.len());
        let mut sources = Vec::with_capacity(req.prefetch.len());
        for (i, (p, &limit)) in req.prefetch.iter().zip(&limits).enumerate() {
            let filter = both(req.filter.as_ref(), p.filter.as_ref());
            let leaf = Leaf {
                query: &p.query,
                filter: filter.as_ref(),
                limit,
                exact: p.exact,
                hnsw_ef: p.hnsw_ef,
                score_threshold: p.score_threshold,
                deadline,
            };
            let (hits, _) = self
                .rank(&view, &leaf)
                .map_err(|e| QueryError::Prefetch(i, e.into()))?;
            sources.push((limit, hits.len()));
            lists.push((p.weight, hits));
        }
        deadline.check()?;
        let fused = req.fusion.fuse(&lists);
        let pool = fused.len();
        let points = fused
            .into_iter()
            .skip(req.offset)
            .take(req.limit)
            .enumerate()
            .map(|(i, s)| {
                deadline.tick(i)?;
                scored(&view, s.key, s.score, &req.with_payload, req.with_vector)
            })
            .collect::<Result<_>>()?;
        deadline.check()?;
        Ok(FusedResult {
            points,
            candidates: Candidates { pool, sources },
        })
    }

    /// One search's best points, best first, with internal scores, and how they read in the
    /// API. The filter must be valid already.
    fn rank(&self, view: &View, leaf: &Leaf<'_>) -> Result<(Vec<Scored<PointId>>, Scale)> {
        // A read that used up its time waiting to start does not start.
        leaf.deadline.check()?;
        let (mut hits, scale) = match leaf.query {
            Query::Vector(vector) => {
                let VectorConfig { dim, distance } = self.shard.config();
                let query = distance.prepare(dim, vector.clone())?;
                (
                    self.rank_vector(view, &query, leaf)?,
                    Scale::Distance(distance),
                )
            }
            Query::Text(q) => (
                text::rank(view, q, leaf.filter, leaf.limit, leaf.deadline)?,
                Scale::Raw,
            ),
            Query::Sparse(q) => (
                sparse::rank(view, q, leaf.filter, leaf.limit, leaf.deadline)?,
                Scale::Raw,
            ),
        };
        // The loops check the clock now and then, so one last look keeps a search that ran over
        // from answering as if it had not.
        leaf.deadline.check()?;
        if let Some(t) = leaf.score_threshold {
            // Hits come best first, so the ones failing the threshold are all at the end.
            let keep = hits
                .iter()
                .take_while(|s| scale.passes(scale.to_user(s.score), t))
                .count();
            hits.truncate(keep);
        }
        Ok((hits, scale))
    }

    /// The best `leaf.limit` points for a prepared vector, best first, with internal scores.
    fn rank_vector(
        &self,
        view: &View,
        query: &[f32],
        leaf: &Leaf<'_>,
    ) -> Result<Vec<Scored<PointId>>> {
        let VectorConfig { dim, distance } = self.shard.config();
        let window = leaf.limit;
        let filter = leaf.filter;
        if window == 0 {
            return Ok(Vec::new());
        }
        let mut top = TopK::new(window);
        for (i, p) in view.recent().enumerate() {
            leaf.deadline.tick(i)?;
            let score = distance.score(query, p.vector);
            if top.would_keep(&p.id, score)
                && filter.map_or(Ok(true), |f| p.payload().map(|pl| f.matches(p.id, &pl)))?
            {
                top.push(p.id, score);
            }
        }
        let ef = leaf.hnsw_ef.unwrap_or(DEFAULT_HNSW_EF).max(window);
        for seg in view.segments() {
            leaf.deadline.check()?;
            let bounds = segment_bounds(&seg, filter);
            let check = |row| -> Result<bool> {
                match filter {
                    Some(f) => Ok(row_matches(&seg, f, row)?),
                    None => Ok(true),
                }
            };
            let vectors = seg.vectors();
            let graph = seg.graph().filter(|_| !leaf.exact);
            let interrupt = || leaf.deadline.check();
            let mut steps = 0usize;
            // Rows are in id order within a segment, so ties cut inside it keep the same ids
            // the merge would.
            let hits = match graph {
                None => search_filtered_until(
                    distance, vectors, dim, query, window, &bounds, check, interrupt,
                )?,
                Some(g) => {
                    let plan = choose(bounds, &self.plan, check)?;
                    let b = &plan.bounds;
                    match plan.strategy {
                        Strategy::Enumerate => search_filtered_until(
                            distance, vectors, dim, query, window, b, check, interrupt,
                        )?,
                        Strategy::Graph => g.search(vectors, query, window, ef, |row| {
                            // Asked about every node good enough to queue, and only a queued
                            // node is ever expanded, so between two ticks the walk scores at
                            // most a thousand or so nodes' links, which paces the time limit
                            // through one big segment.
                            steps += 1;
                            leaf.deadline.tick(steps)?;
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
        Ok(top.into_sorted_vec())
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

/// One search to rank points by, with everything checked already.
struct Leaf<'a> {
    query: &'a Query,
    filter: Option<&'a Filter>,
    /// How many of the best points to return.
    limit: usize,
    exact: bool,
    hnsw_ef: Option<usize>,
    score_threshold: Option<f32>,
    deadline: Deadline,
}

/// `offset + limit`, checked against [`MAX_RESULT_WINDOW`].
fn check_window(offset: usize, limit: usize) -> Result<usize> {
    let window = offset.saturating_add(limit);
    if window > MAX_RESULT_WINDOW {
        return Err(QueryError::field(
            "limit",
            format!("offset plus limit may be at most {MAX_RESULT_WINDOW}, got {window}"),
        ));
    }
    Ok(window)
}

/// Checks a vector search's beam width, which other searches ignore.
fn check_ef(query: &Query, hnsw_ef: Option<usize>) -> Result<()> {
    if let Some(ef) = hnsw_ef
        && matches!(query, Query::Vector(_))
        && !(1..=MAX_HNSW_EF).contains(&ef)
    {
        return Err(QueryError::field(
            "hnsw_ef",
            format!("hnsw_ef must be between 1 and {MAX_HNSW_EF}, got {ef}"),
        ));
    }
    Ok(())
}

/// Checks a score threshold is a finite number.
fn check_threshold(threshold: Option<f32>) -> Result<()> {
    match threshold {
        Some(t) if !t.is_finite() => Err(QueryError::field(
            "score_threshold",
            format!("score_threshold must be a finite number that fits in 32 bits, got {t}"),
        )),
        _ => Ok(()),
    }
}

/// Checks a prefetch and returns its limit, `window` when it gives none.
fn check_prefetch(p: &Prefetch, root: Option<&Filter>, window: usize) -> Result<usize> {
    check_weight(p.weight).map_err(|e| QueryError::field("weight", e))?;
    let limit = p.limit.unwrap_or(window);
    if !(1..=MAX_RESULT_WINDOW).contains(&limit) {
        return Err(QueryError::field(
            "limit",
            format!("limit must be between 1 and {MAX_RESULT_WINDOW}, got {limit}"),
        ));
    }
    check_ef(&p.query, p.hnsw_ef)?;
    check_threshold(p.score_threshold)?;
    if let Some(f) = &p.filter {
        f.validate()?;
        // The two filters run as one, which has to stay within the limits too.
        if let Some(both) = both(root, Some(f))
            && let Err(e) = both.validate()
        {
            let reason = match e {
                nocturnum_core::Error::InvalidFilter { reason, .. } => reason,
                e => e.to_string(),
            };
            return Err(QueryError::field(
                "filter",
                format!("together with the query's filter, {reason}"),
            ));
        }
    }
    Ok(limit)
}

/// Both filters at once, each already checked on its own.
fn both(a: Option<&Filter>, b: Option<&Filter>) -> Option<Filter> {
    match (a, b) {
        (Some(a), Some(b)) => Some(Filter {
            must: vec![Condition::Filter(a.clone()), Condition::Filter(b.clone())],
            ..Filter::default()
        }),
        (a, b) => a.or(b).cloned(),
    }
}

/// A result with as much of its point as the request asks for.
fn scored(
    view: &View,
    id: PointId,
    score: f32,
    with_payload: &WithPayload,
    with_vector: bool,
) -> Result<ScoredPoint> {
    let (payload, vector, sparse) = if *with_payload == WithPayload::None && !with_vector {
        (None, None, None)
    } else {
        let rec = view
            .get(id)?
            .expect("the view still holds every point it scored");
        (
            project(&rec.payload, with_payload),
            with_vector.then_some(rec.vector),
            rec.sparse.filter(|_| with_vector),
        )
    };
    Ok(ScoredPoint {
        id,
        sparse,
        score,
        payload,
        vector,
    })
}

/// How a ranking's internal scores, where higher is always better, read in the API.
#[derive(Debug, Clone, Copy)]
enum Scale {
    Distance(Distance),
    /// Scores the API reports as they are, higher is better: BM25 and sparse dot products.
    Raw,
}

impl Scale {
    fn to_user(self, score: f32) -> f32 {
        match self {
            Scale::Distance(d) => d.to_user(score),
            Scale::Raw => score,
        }
    }

    fn higher_is_better(self) -> bool {
        match self {
            Scale::Distance(d) => d.user_higher_is_better(),
            Scale::Raw => true,
        }
    }

    /// Whether an API score passes a threshold, at least it for similarities and text, at
    /// most it for distances.
    fn passes(self, score: f32, threshold: f32) -> bool {
        if self.higher_is_better() {
            score >= threshold
        } else {
            score <= threshold
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
    view: &View,
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
                return Err(QueryError::field(
                    "with_payload",
                    format!("payload paths to include or exclude must be object keys, {p} is not"),
                ));
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
mod fused_tests;
mod recall;
mod sparse;
#[cfg(test)]
mod tests;
mod text;

pub use sparse::SparseQuery;

pub use text::MAX_QUERY_TERMS;
