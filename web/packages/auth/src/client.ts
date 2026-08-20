// Thin, well-typed auth client for the BFF — a small ergonomic surface over the
// generated `@amicu/api-client` (OpenAPI-derived, from `crates/amicu-schema/
// openapi.json`).
//
// Resolved amicu-planning#83: the request/response *shapes* below are no longer
// hand-written. `TokenResponse` and every request body are typed straight from the
// generated schema, so this client cannot drift from the spec. What remains here is
// only ergonomics: a compact method-per-flow surface returning a discriminated
// `AuthResult`, so callers (server actions / middleware) don't handle openapi-fetch's
// `{ data, error, response }` triples directly.
//
// Server-only: these calls carry credentials / refresh tokens and must never run
// in the browser. They are invoked exclusively from the BFF (server actions and
// route handlers).

import { createApiClient, type TokenResponse } from '@amicu/api-client';

export type { TokenResponse };

export type AuthResult<T> =
  | { ok: true; data: T }
  | { ok: false; status: number; code?: string };

export interface AuthClientOptions {
  /** Base URL of the auth issuer / Management API (server-only env `API_URL`). */
  baseUrl: string;
  /** Optional fetch override (tests / instrumentation). */
  fetch?: typeof fetch;
}

export interface AuthClient {
  /** Exchange email+password for a token bundle. Maps to `POST /token`. */
  login(input: { email: string; password: string }): Promise<AuthResult<TokenResponse>>;
  /**
   * Register a new account. Maps to `POST /signup`.
   *
   * The backend ALWAYS responds `202` (with a generic `{ message }` we discard) and
   * never 409 — it does not reveal whether the email already exists
   * (anti-enumeration). So this is a fire-and-forget acknowledgement: it does NOT
   * log the user in. The account is confirmed out-of-band (email), after which the
   * user signs in.
   */
  signup(input: { email: string; password: string }): Promise<AuthResult<void>>;
  /** Rotate a refresh token for a fresh bundle. Maps to `POST /token/refresh`. */
  refresh(refreshToken: string): Promise<AuthResult<TokenResponse>>;
  /** Revoke a refresh token (idempotent, `204`). Maps to `POST /logout`. */
  logout(refreshToken: string): Promise<AuthResult<void>>;
  /**
   * Request a password-reset email. Maps to `POST /recover`; the backend replies
   * `202` with a generic `{ message }` whether or not the email is registered
   * (anti-enumeration), which the BFF treats uniformly as success.
   */
  requestPasswordReset(input: { email: string }): Promise<AuthResult<void>>;
}

export function createAuthClient(options: AuthClientOptions): AuthClient {
  const api = createApiClient({ baseUrl: options.baseUrl, fetch: options.fetch });

  // Normalize openapi-fetch's `{ data, error, response }` into `AuthResult`, and
  // turn a thrown fetch (network/DNS failure) into a 503-like result the caller can
  // genericize — never a rejected promise.
  async function toResult<T>(
    call: Promise<{ data?: unknown; error?: unknown; response: Response }>,
    map: (data: unknown) => T,
  ): Promise<AuthResult<T>> {
    let res: { error?: unknown; response: Response; data?: unknown };
    try {
      res = await call;
    } catch {
      return { ok: false, status: 503 };
    }
    if (!res.response.ok) {
      return { ok: false, status: res.response.status, code: errorCode(res.error) };
    }
    return { ok: true, data: map(res.data) };
  }

  // Spec error responses (401/429/400/…) carry no body, so `code` is usually
  // absent; if a future error body includes one we surface it.
  function errorCode(error: unknown): string | undefined {
    if (error && typeof error === 'object') {
      const e = error as { code?: string; error_code?: string; error?: string };
      return e.code ?? e.error_code ?? e.error;
    }
    return undefined;
  }

  return {
    login: (input) => toResult(api.POST('/token', { body: input }), (d) => d as TokenResponse),
    signup: (input) => toResult(api.POST('/signup', { body: input }), () => undefined),
    refresh: (refreshToken) =>
      toResult(api.POST('/token/refresh', { body: { refresh_token: refreshToken } }), (d) => d as TokenResponse),
    logout: (refreshToken) =>
      toResult(api.POST('/logout', { body: { refresh_token: refreshToken } }), () => undefined),
    requestPasswordReset: (input) => toResult(api.POST('/recover', { body: input }), () => undefined),
  };
}
