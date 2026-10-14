//! Shared parse → validate → plan orchestration.
//!
//! This crate lifts the graph-level engine glue that previously lived in
//! `amicu-cli` (`catalog`, `checks`, `planner`) into a reusable library surface
//! so both the CLI and the HTTP API drive the same validation and planning
//! logic. It is deliberately side-effect-free: nothing here reads local state,
//! touches the filesystem, resolves secrets, or launches processes - those
//! concerns stay in the CLI (`apply`) and are out of scope for the stateless
//! API endpoints.
//!
//! The pieces:
//! - [`parse_config`] - parse a single HCL config document into a
//!   [`Graph`](amicu_core::graph::Graph).
//! - [`report_for`] - run structural and per-deployment validation, producing a
//!   [`DiagnosticReport`](amicu_core::diagnostic::DiagnosticReport).
//! - [`build_plan`] - diff a graph against optional prior state into an
//!   [`amicu_plan::Plan`] plus the launch artifacts `apply` needs.

pub mod catalog;
pub mod checks;
pub mod fingerprint;
pub mod planner;

use amicu_core::graph::Graph;
use amicu_hcl::{HclError, parse_graph};

pub use catalog::build_catalog;
pub use checks::report_for;
pub use fingerprint::{EnvProbe, Fact, Fingerprint, NoProbe};
pub use planner::{
    PlanError, PlanInputs, PlanItem, PlanResult, build_plan, build_plan_in, build_plan_with,
};

/// Parse a single HCL configuration document into the canonical graph.
///
/// Unlike the CLI's multi-file project discovery, this takes one text blob
/// (as an HTTP request carries), so it is the parse entry point the API uses.
///
/// # Errors
/// Returns [`HclError`] when the text is not valid Amicu HCL.
pub fn parse_config(text: &str) -> Result<Graph, HclError> {
    parse_graph(text)
}
