//! Planning catalog construction.
//!
//! The implementation now lives in `amicu-engine` so the API and CLI share one
//! parse → validate → plan surface; this is a thin re-export kept for the
//! `crate::catalog` call sites (e.g. `apply`).

pub(crate) use amicu_engine::catalog::build_catalog;
