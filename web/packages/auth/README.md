# @amicu/auth

Client-side session/tier helpers for the web app, consuming the **TASK-078**
auth service (andypalmi/amicu-planning#78). Implements RFC 0001 §6–§7 and
decision §11.6.

Two entry points keep server-only code out of client bundles:

- **`@amicu/auth`** (client-safe) - the tier model (`Tier`, `tierSatisfies`, …),
  claim/session types (`AccessClaims`, `SessionUser`), and zod form validation.
  Safe to import from client components.
- **`@amicu/auth/server`** (server-only) - offline JWKS verification of the
  short-lived access JWT (`verifyAccessToken`, via `jose`) and a thin auth API
  client (`createAuthClient`) wrapping the issuer's `/token`, `/token/refresh`,
  `/logout`, `/signup` endpoints. Only ever runs in the BFF (server actions /
  route handlers / middleware).

Tier claims are read **for UX gating only, never for enforcement** - the Rust
Management API re-checks every mutation.

## Not yet done here

- The auth API client is hand-written against TASK-078's endpoint shapes and is
  **TODO-marked to be replaced by the generated `@amicu/api-client`**
  (andypalmi/amicu-planning#83) once the OpenAPI spec lands.
- Password-reset (`/recover`) is not in TASK-078's acceptance criteria yet - the
  wrapper and UI exist, but the backend endpoint needs confirming.
