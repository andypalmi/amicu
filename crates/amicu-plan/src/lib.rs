//! Diff, plan, policy output.
//!
//! This crate defines the stable [`Plan`](contract::Plan) JSON contract that
//! `amicu plan` renders and that the API and UI consume.

pub mod contract;

pub use contract::Plan;
