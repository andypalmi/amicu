// @amicu/auth - client-safe entry point.
//
// Holds the tier model, claim/session types, and form validation used by both the
// browser (client components) and the BFF. Anything requiring `jose`, network
// access, or credentials lives in the server-only subpath `@amicu/auth/server`
// and must NOT be re-exported here (it would pull server code into client bundles).
//
// Consumes the TASK-078 auth service; see ./README.md and RFC 0001 §6–§7.

export {
  TIERS,
  TIER_LABELS,
  TIER_ORDER,
  tierRank,
  isTier,
  tierSatisfies,
  type Tier,
} from './tier';

export {
  toSessionUser,
  normalizeClaims,
  type AccessClaims,
  type SessionUser,
  type Role,
} from './claims';

export {
  loginSchema,
  signupSchema,
  forgotSchema,
  fieldErrors,
  type LoginInput,
  type SignupInput,
  type ForgotInput,
  type FieldErrors,
} from './validation';
