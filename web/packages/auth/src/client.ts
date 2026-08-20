// Thin, well-typed auth client — fetch wrappers over the TASK-078 auth endpoints.
//
// TODO(amicu-planning#83): replace this hand-written client with the generated
// `@amicu/api-client` (OpenAPI-derived) once it lands. Keep this surface small and
// isolated so the swap is a drop-in. The request/response shapes below are the
// project's best reading of TASK-078 / RFC 0001 §6 and MUST be reconciled against
// the committed OpenAPI spec — see the "backend gaps" note in the PR.
//
// Server-only: these calls carry credentials / refresh tokens and must never run
// in the browser. They are invoked exclusively from the BFF (server actions and
// route handlers).

/** Token bundle returned by the issuer. */
export interface TokenResponse {
  access_token: string;
  refresh_token: string;
  /** Access-token lifetime in seconds, when the issuer reports it. */
  expires_in?: number;
  token_type?: string;
}

export type AuthResult<T> =
  | { ok: true; data: T }
  | { ok: false; status: number; code?: string };

export interface AuthClientOptions {
  /** Base URL of the auth issuer / Management API (server-only env `API_URL`). */
  baseUrl: string;
  /** Optional fetch override (tests / instrumentation). */
  fetch?: typeof fetch;
}

async function readError(res: Response): Promise<string | undefined> {
  try {
    const body = (await res.json()) as { error?: string; code?: string; error_code?: string };
    return body.code ?? body.error_code ?? body.error;
  } catch {
    return undefined;
  }
}

export interface AuthClient {
  /** Exchange email+password for a token bundle. Maps to `POST /token`. */
  login(input: { email: string; password: string }): Promise<AuthResult<TokenResponse>>;
  /**
   * Register a new account. Maps to `POST /signup`.
   *
   * The backend ALWAYS responds `202 { message }` with NO tokens and never 409 —
   * it does not reveal whether the email already exists (anti-enumeration). So
   * this is a fire-and-forget acknowledgement: it does NOT log the user in. The
   * account is confirmed out-of-band (email), after which the user signs in.
   */
  signup(input: { email: string; password: string }): Promise<AuthResult<void>>;
  /** Rotate a refresh token for a fresh bundle. Maps to `POST /token/refresh`. */
  refresh(refreshToken: string): Promise<AuthResult<TokenResponse>>;
  /** Revoke a refresh token. Maps to `POST /logout`. */
  logout(refreshToken: string): Promise<AuthResult<void>>;
  /**
   * Request a password-reset email. Maps to `POST /recover` (GoTrue-style).
   * NOTE: not enumerated in TASK-078's acceptance criteria — flagged as a backend
   * gap in the PR. The BFF treats every outcome as success (anti-enumeration).
   */
  requestPasswordReset(input: { email: string }): Promise<AuthResult<void>>;
}

export function createAuthClient(options: AuthClientOptions): AuthClient {
  const doFetch = options.fetch ?? fetch;
  const base = options.baseUrl.replace(/\/+$/, '');

  async function postJson<T>(
    path: string,
    body: unknown,
    parse: (res: Response) => Promise<T>,
  ): Promise<AuthResult<T>> {
    let res: Response;
    try {
      res = await doFetch(`${base}${path}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', accept: 'application/json' },
        body: JSON.stringify(body),
        // Never send ambient browser cookies to the issuer; the BFF owns the session.
        cache: 'no-store',
      });
    } catch {
      // Network/DNS failure — surface as a 503-like result the caller can genericize.
      return { ok: false, status: 503 };
    }
    if (!res.ok) {
      return { ok: false, status: res.status, code: await readError(res) };
    }
    return { ok: true, data: await parse(res) };
  }

  const asTokens = (res: Response) => res.json() as Promise<TokenResponse>;
  const asVoid = async () => undefined;

  return {
    login: (input) =>
      postJson('/token', { grant_type: 'password', ...input }, asTokens),
    // 202 ack, no token body — parse as void.
    signup: (input) => postJson('/signup', input, asVoid),
    refresh: (refreshToken) =>
      postJson('/token/refresh', { refresh_token: refreshToken }, asTokens),
    logout: (refreshToken) =>
      postJson('/logout', { refresh_token: refreshToken }, asVoid),
    requestPasswordReset: (input) => postJson('/recover', input, asVoid),
  };
}
