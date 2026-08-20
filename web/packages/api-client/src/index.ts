// @amicu/api-client — the generated TypeScript client for the Amicu Management API.
//
// Source of truth: `crates/amicu-schema/openapi.json`. The types in `./schema` are
// GENERATED (never hand-edited); regenerate with `pnpm --filter @amicu/api-client
// generate`, and CI fails on any drift (`check:generated`). See ./README.md.

export { createApiClient, type ApiClient, type ApiClientOptions } from './client';

// Re-export the generated schema so consumers get one import for both the runtime
// client and its request/response types. `paths`/`components`/`operations` are the
// raw generated shapes; the named aliases below are the ergonomic ones.
export type { paths, components, operations } from './schema';
export type {
  Credentials,
  TokenResponse,
  RefreshRequest,
  SignUpResponse,
  RecoverRequest,
  RecoverResponse,
  ResetRequest,
  ValidateRequest,
  PlanRequest,
  Plan,
  DiagnosticReport,
  RecommendRequest,
  TuningReportDto,
  RecommendationDto,
  MonitorRequest,
  MonitorViewDto,
  StackMetricsDto,
  WorkloadEstimateDto,
  CheckoutRequest,
  RedirectResponse,
  BillingStatusResponse,
  MemberView,
  MembersResponse,
  AddMemberRequest,
  ChangeRoleRequest,
  RoleView,
  RolesResponse,
  AuditEntryView,
  AuditResponse,
} from './schema';
