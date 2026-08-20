//! The single aggregated OpenAPI document for the Management API.
//!
//! [`ApiDoc`] is the source of truth: the auth surface is documented here via
//! its annotated handlers, and the engine endpoints (`/validate`, `/plan`) are
//! merged in at router-build time (see [`crate::api_openapi`]) so one document
//! describes everything the service serves. It is committed to
//! `crates/amicu-schema/openapi.json` for web codegen and CI drift detection.

use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

/// Aggregated API document (auth paths + shared component schemas).
#[derive(OpenApi)]
#[openapi(
    info(
        title = "Amicu Management API",
        description = "First-party auth plus the stateless validate/plan engine endpoints.",
        version = "0.1.0",
        license(name = "AGPL-3.0-only"),
    ),
    paths(
        crate::auth::sign_up,
        crate::auth::token,
        crate::auth::refresh,
        crate::auth::logout,
        crate::auth::recover,
        crate::auth::reset,
        crate::auth::jwks,
        crate::auth::openid_configuration,
    ),
    components(schemas(
        crate::auth::Credentials,
        crate::auth::RefreshRequest,
        crate::auth::TokenResponse,
        crate::auth::SignUpResponse,
        crate::auth::RecoverRequest,
        crate::auth::RecoverResponse,
        crate::auth::ResetRequest,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "auth", description = "Sign-up, token issuance/rotation, logout, and JWKS discovery."),
        (name = "engine", description = "Stateless, side-effect-free validate and plan endpoints (auth-gated)."),
        (name = "recommend", description = "Stateless advisory tuning recommendations from a metrics + estimate snapshot (auth-gated, tier-appropriate)."),
    ),
)]
pub struct ApiDoc;

/// Registers the `bearer` (JWT) HTTP security scheme referenced by the
/// auth-gated engine paths.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        // `components` is always present: the derive registers component schemas.
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("JWT")
                        .build(),
                ),
            );
        }
    }
}
