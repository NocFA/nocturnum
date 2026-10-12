//! Query execution: filter planning, candidate search across segments, scoring, rescoring and fusion.

pub mod collection;
pub mod plan;

pub use collection::{
    Collection, Point, QueryError, RecallReport, RecallRequest, RecallRun, ScoredPoint, ScrollPage,
    ScrollRequest, SearchRequest, Selector, WithPayload,
};
pub use plan::{Estimate, Plan, PlanOptions, Strategy};
