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
        crate::auth::verify,
        crate::auth::resend_verification,
        crate::auth::jwks,
        crate::auth::openid_configuration,
        crate::billing::checkout_handler,
        crate::billing::portal_handler,
        crate::billing::status_handler,
        crate::billing::webhook_handler,
        crate::org::list_members_handler,
        crate::org::add_member_handler,
        crate::org::change_role_handler,
        crate::org::remove_member_handler,
        crate::org::list_roles_handler,
        crate::org::audit_handler,
        crate::config::create_config_handler,
        crate::config::list_configs_handler,
        crate::config::get_config_handler,
        crate::config::list_versions_handler,
        crate::config::get_version_handler,
        crate::config::append_version_handler,
        crate::config::rollback_handler,
        crate::config::diff_handler,
        crate::session::list_workspaces_handler,
        crate::session::switch_workspace_handler,
        crate::tenancy::create_workspace_handler,
        crate::tenancy::create_invite_handler,
        crate::tenancy::list_invites_handler,
        crate::tenancy::accept_invite_handler,
    ),
    components(schemas(
        crate::auth::Credentials,
        crate::auth::RefreshRequest,
        crate::auth::TokenResponse,
        crate::auth::SignUpResponse,
        crate::auth::RecoverRequest,
        crate::auth::RecoverResponse,
        crate::auth::ResetRequest,
        crate::auth::VerifyRequest,
        crate::auth::ResendVerificationRequest,
        crate::auth::ResendVerificationResponse,
        crate::billing::CheckoutRequest,
        crate::billing::RedirectResponse,
        crate::billing::BillingStatusResponse,
        crate::org::MemberView,
        crate::org::MembersResponse,
        crate::org::AddMemberRequest,
        crate::org::ChangeRoleRequest,
        crate::org::RoleView,
        crate::org::RolesResponse,
        crate::org::AuditEntryView,
        crate::org::AuditResponse,
        crate::config::CreateConfigRequest,
        crate::config::AppendVersionRequest,
        crate::config::ConfigSummaryDto,
        crate::config::ConfigDetailDto,
        crate::config::VersionMetaDto,
        crate::config::VersionContentDto,
        crate::config::ConfigDiffDto,
        crate::session::WorkspaceView,
        crate::session::WorkspacesResponse,
        crate::session::SwitchWorkspaceRequest,
        crate::tenancy::CreateWorkspaceRequest,
        crate::tenancy::CreateWorkspaceResponse,
        crate::tenancy::CreateInviteRequest,
        crate::tenancy::CreateInviteResponse,
        crate::tenancy::InviteView,
        crate::tenancy::InvitesResponse,
        crate::tenancy::AcceptInviteRequest,
        crate::tenancy::AcceptInviteResponse,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "auth", description = "Sign-up, token issuance/rotation, logout, and JWKS discovery."),
        (name = "engine", description = "Stateless, side-effect-free validate and plan endpoints (auth-gated)."),
        (name = "recommend", description = "Stateless advisory tuning recommendations from a metrics + estimate snapshot (auth-gated, tier-appropriate)."),
        (name = "monitor", description = "Stateless monitoring view (measured-vs-estimated): a normalized metric surface, estimate, and tier-appropriate advisory suggestions (auth-gated)."),
        (name = "preset", description = "Stateless guided-wizard presets: gallery listing, per-preset detail, and deterministic HCL emit from wizard answers (auth-gated, Community-usable)."),
        (name = "billing", description = "Stripe-backed subscription billing: checkout, customer portal, status, and the signature-authenticated webhook."),
        (name = "org", description = "Enterprise-gated organization console: membership, role-based access control, and an append-only audit log."),
        (name = "config", description = "HCL config storage and version history: create/list/get, append-only version history, rollback (mints a new version, never rewrites), and a server-computed unified diff. Content is canonicalized before it is stored (auth-gated, Community-usable)."),
        (name = "session", description = "Workspace claim in tokens: list the authenticated caller's workspace memberships and switch the active workspace (mints a fresh token pair scoped to the target only if the caller is a member; 403 otherwise)."),
        (name = "tenancy", description = "Multi-tenant provisioning and invitations: create additional workspaces (server-generated id) and mint/list/accept workspace invites. Membership binds server-side to the invite's workspace and role only."),
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
