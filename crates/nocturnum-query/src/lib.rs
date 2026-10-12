//! Query execution: filter planning, candidate search across segments, scoring, rescoring and fusion.

pub mod collection;
pub mod deadline;
pub mod fusion;
pub mod plan;

pub use collection::{
    Candidates, Collection, FusedRequest, FusedResult, Point, Prefetch, Query, QueryError,
    RecallReport, RecallRequest, RecallRun, ScoredPoint, ScrollPage, ScrollRequest, SearchRequest,
    Selector, SparseQuery, TextQuery, WithPayload,
};
pub use deadline::Deadline;
pub use fusion::Fusion;
pub use nocturnum_index::text::Bm25;
pub use plan::{Estimate, Plan, PlanOptions, Strategy};
