//! Diagnostic assembly.
//!
//! The implementation now lives in `amicu-engine` so the API and CLI share one
//! parse → validate → plan surface; this is a thin re-export kept for the
//! `crate::checks` call sites.

pub(crate) use amicu_engine::checks::report_for;
