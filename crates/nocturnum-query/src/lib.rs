//! Query execution: filter planning, candidate search across segments, scoring, rescoring and fusion.

pub mod plan;

pub use plan::{Estimate, Plan, PlanOptions, Strategy};
