//! HTTP surface for the deterministic "customize" flow: `POST
//! /recommend/customize` (andypalmi/amicu-planning#35).
//!
//! A wizard candidate's generated HCL (see [`crate::recommend_wizard::CandidateDto::hcl`])
//! is a starting point, not a final answer: a user may hand-edit it before
//! accepting it. This endpoint re-parses the (possibly edited) text, runs the
//! same structural + per-deployment validation `POST /validate` runs, and
//! diffs the resulting graph against the candidate's original graph (its
//! `base_graph`) into a [`Plan`] - exactly the `POST /plan` orchestration,
//! reused rather than reimplemented. There is no separate "diff" type:
//! [`amicu_engine::build_plan`]'s create/update/destroy actions against
//! `base_graph` as prior *are* the graph diff. See [`amicu_plan::Plan`] for
//! the returned contract.
//!
//! This is deliberately **not** natural-language or LLM-driven: no
//! interpretation of intent happens here, only deterministic
//! parse → validate → plan over text the user (or a prior wizard step)
//! supplied. It is also plain synchronous JSON, not SSE - but
//! [`CustomizeResponseDto`]'s fields (`report`, `plan`, `graph`, `hcl`) are
//! shaped so that a later SSE wrapper could emit each as its own terminal
//! event (e.g. a `report` event, then a `plan` event) without changing the
//! contract; this endpoint is that wrapper's eventual synchronous fallback,
//! not a competing shape.
//!
//! **Known v1 limitation**: the response never carries a revised `estimate`
//! or `confidence` for the edited HCL. [`amicu_recommend`]'s scoring only
//! runs over the wizard's structured (preset, answers) input, not over
//! arbitrary graphs reverse-engineered from free-edited HCL, so fabricating
//! numbers here would misrepresent them as engine-derived. Follow-up work may
//! add a best-effort re-estimate; until then a client should treat the
//! *last-known* estimate/confidence (from the candidate that produced this
//! HCL) as stale once the user has edited the text.
//!
//! ## Why a parse failure is `422`, not `400` (unlike `POST /plan`)
//!
//! `POST /plan`'s config text is developer/CLI input: a syntax error is a
//! plain client mistake, so it's a `400` with a bare error string. The
//! customize flow's `hcl` is user-edited prose in a UI that always renders a
//! `DiagnosticsPanel` from the response's `report` field - forcing that UI to
//! also handle a separate bare-string `400` error body just for "didn't
//! parse" would be a second code path for what the user experiences as the
//! same thing ("here's what's wrong with your HCL"). So a parse failure here
//! is folded into the same [`DiagnosticReport`] shape as every other
//! validation failure, under the new generic
//! [`catalog::PARSE_INVALID_SYNTAX`] code, and returned as `422` - never
//! `400`, never `500`, never a silent `200`. The same code and status also
//! cover an [`amicu_hcl::format_graph`] rejection (a resource id or reference
//! that parsed but is not a valid, round-trip-safe HCL identifier): that check
//! only runs at re-render time, not at parse time, so it is a genuinely
//! reachable user-editing mistake and not merely a theoretical internal
//! failure - see [`hcl_error_response`].
//!
//! An unparsable `base_graph`, by contrast, is not something a
//! `DiagnosticsPanel` renders at all (it's not user-edited text - the client
//! sends back opaque JSON it received verbatim from `/recommend/candidates`
//! moments earlier). A malformed `base_graph` is therefore a plain `400`
//! with a `{"error": "..."}` body, consistent with how other handlers in this
//! crate reject a malformed request body (e.g.
//! [`crate::preset::PresetHttpError::BadAnswer`]).

use amicu_core::diagnostic::{Diagnostic, DiagnosticReport, Severity, catalog};
use amicu_core::graph::Graph;
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::dto::{CustomizeRequestDto, CustomizeResponseDto};
use crate::engine::EngineApiState;

/// Error surfaced by [`customize_handler`].
///
/// Distinct from [`crate::engine::EngineHttpError`]: an `hcl` parse failure
/// is *not* a variant here (it is folded into the `422` [`DiagnosticReport`]
/// response body, per this module's doc-comment) - only failures that are
/// not representable as a diagnostic reach this type.
pub(crate) enum CustomizeHttpError {
    /// `base_graph` is present but is not valid JSON for a
    /// [`Graph`](amicu_core::graph::Graph) (bad request - the client's own
    /// malformed input, echoed back since it reveals nothing about server
    /// internals).
    BadBaseGraph(serde_json::Error),
    /// A runtime adapter failed while planning (internal).
    Plan(amicu_core::adapter::AdapterError),
}

impl From<amicu_core::adapter::AdapterError> for CustomizeHttpError {
    fn from(e: amicu_core::adapter::AdapterError) -> Self {
        Self::Plan(e)
    }
}

impl IntoResponse for CustomizeHttpError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            CustomizeHttpError::BadBaseGraph(e) => {
                (StatusCode::BAD_REQUEST, format!("base_graph: {e}"))
            }
            // Internal failure: log the real cause server-side, return a
            // fixed generic body so nothing about the internals leaks,
            // mirroring `EngineHttpError::Plan`.
            CustomizeHttpError::Plan(e) => {
                eprintln!("amicu-api: customize plan failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Build the `422` response for an `hcl` that [`amicu_hcl`] rejected: a
/// [`DiagnosticReport`] carrying a single [`catalog::PARSE_INVALID_SYNTAX`]
/// error, with the raw submitted text echoed back verbatim (there is nothing
/// to canonically reformat).
///
/// Covers two distinct [`amicu_hcl::HclError`] sources, folded into the same
/// response shape because a `DiagnosticsPanel`-first UI treats both as "here's
/// what's wrong with your HCL":
/// - [`amicu_hcl::parse_graph`] rejecting text that is not syntactically
///   valid HCL, or does not match the resource schema, at all.
/// - [`amicu_hcl::format_graph`] rejecting a graph that parsed fine but
///   carries a resource id (or reference) that is not a valid, round-trip-safe
///   HCL identifier - e.g. a label with a space or a leading digit. Nothing
///   downstream of `parse_graph` checks this, so it is a genuinely reachable
///   user-editing mistake, not a defensive/theoretical case.
fn hcl_error_response(err: &amicu_hcl::HclError, raw_hcl: String) -> Response {
    let report = DiagnosticReport::new(vec![Diagnostic::new(
        catalog::PARSE_INVALID_SYNTAX,
        Severity::Error,
        err.to_string(),
    )]);
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(CustomizeResponseDto {
            report,
            plan: None,
            graph: None,
            hcl: raw_hcl,
        }),
    )
        .into_response()
}

/// Customize (re-parse, validate, and plan) a wizard candidate's HCL.
///
/// The deterministic customize flow: parse `hcl`, deserialize the optional
/// opaque `base_graph`, validate the parsed graph
/// ([`amicu_engine::report_for`]), then diff it against `base_graph`
/// ([`amicu_engine::build_plan`]) exactly as `POST /plan` does for its
/// `config`/`prior`. See the module doc-comment for the full `422`-vs-`400`
/// rationale, the SSE-shape-compatibility note, and the v1
/// no-estimate/confidence limitation.
///
/// Response status by outcome:
/// - `hcl` does not parse, or parses but carries a resource id/reference that
///   is not a valid, round-trip-safe HCL identifier: `422`, `report` carries
///   one [`catalog::PARSE_INVALID_SYNTAX`] diagnostic, `plan`/`graph` absent,
///   `hcl` echoes the raw submission.
/// - `base_graph` is present but not a valid `Graph`: `400`,
///   `{"error": "..."}`.
/// - `hcl` parses but fails structural/per-deployment validation: `422`,
///   `report.valid == false`, `plan`/`graph` absent, `hcl` is the
///   canonically-reformatted text.
/// - `hcl` parses and validates but planning surfaces an error-severity
///   diagnostic (e.g. an adapter-level capability mismatch): `422`, `report`
///   carries the planning diagnostics, `plan`/`graph` absent.
/// - Otherwise: `200`, `report` (its diagnostics, if any, are all
///   non-error), `plan`, and `graph` all present.
#[utoipa::path(
    post,
    path = "/recommend/customize",
    tag = "wizard",
    request_body = CustomizeRequestDto,
    responses(
        (status = 200, description = "The submitted HCL parsed, validated, and planned cleanly.", body = CustomizeResponseDto),
        (status = 400, description = "`base_graph` is present but is not valid JSON for a Graph."),
        (status = 401, description = "Missing or invalid access token."),
        (status = 403, description = "Access tier insufficient."),
        (status = 422, description = "The HCL did not parse, failed validation, or failed to plan; body carries the blocking DiagnosticReport.", body = CustomizeResponseDto),
    ),
    security(("bearer" = [])),
)]
pub(crate) async fn customize_handler(
    State(_state): State<EngineApiState>,
    Json(body): Json<CustomizeRequestDto>,
) -> Result<Response, CustomizeHttpError> {
    let graph = match amicu_hcl::parse_graph(&body.hcl) {
        Ok(graph) => graph,
        Err(e) => return Ok(hcl_error_response(&e, body.hcl)),
    };

    let base_graph = match body.base_graph {
        Some(v) => {
            Some(serde_json::from_value::<Graph>(v).map_err(CustomizeHttpError::BadBaseGraph)?)
        }
        None => None,
    };

    // `parse_graph` does not check identifier round-trip-safety (only
    // `format_graph` does), so a graph that just parsed successfully can
    // still fail to re-render - e.g. a resource label with a space or a
    // leading digit. That is a genuinely reachable user-editing mistake, not
    // an internal error, so it is folded into the same 422 diagnostic path as
    // an outright parse failure (see `hcl_error_response`).
    let canonical_hcl = match amicu_hcl::format_graph(&graph) {
        Ok(hcl) => hcl,
        Err(e) => return Ok(hcl_error_response(&e, body.hcl)),
    };

    let report = amicu_engine::report_for(&graph);
    if !report.valid {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(CustomizeResponseDto {
                report,
                plan: None,
                graph: None,
                hcl: canonical_hcl,
            }),
        )
            .into_response());
    }

    let result = amicu_engine::build_plan(&graph, base_graph.as_ref())?;
    if result.has_errors() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(CustomizeResponseDto {
                report: DiagnosticReport::new(result.diagnostics),
                plan: None,
                graph: None,
                hcl: canonical_hcl,
            }),
        )
            .into_response());
    }

    let graph_json = serde_json::to_value(&graph).expect("a Graph always serializes to JSON");
    Ok(Json(CustomizeResponseDto {
        report: DiagnosticReport::new(result.diagnostics),
        plan: Some(result.plan),
        graph: Some(graph_json),
        hcl: canonical_hcl,
    })
    .into_response())
}
