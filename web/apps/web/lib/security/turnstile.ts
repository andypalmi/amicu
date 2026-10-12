// Cloudflare Turnstile server-side verification ("siteverify").
//
// Pure and dependency-free (no `server-only`, no next/*) so it runs under
// `node --test`. Callers inject env + fetch.
//
// Policy:
//  - Env-gated: with no TURNSTILE_SECRET_KEY the check is skipped (dev works).
//  - FAIL-CLOSED: if the secret is set but the token is missing, Cloudflare is
//    unreachable / times out / returns non-2xx, the request is rejected. A
//    fail-open policy would let an attacker who can degrade the siteverify call
//    (or simply omit the token) bypass the protection entirely, and these forms
//    (signup, password reset, resend) are exactly the abuse targets. The cost of
//    a Cloudflare outage is that those three forms briefly show a generic
//    "try again" error; login and the rest of the app are unaffected.
//  - Errors never distinguish *why* to the end user and never depend on the
//    email, so anti-enumeration is untouched.

export const SITEVERIFY_URL = 'https://challenges.cloudflare.com/turnstile/v0/siteverify';
export const TURNSTILE_RESPONSE_FIELD = 'cf-turnstile-response';
const MAX_TOKEN_LENGTH = 2048;
const DEFAULT_TIMEOUT_MS = 5000;

export type TurnstileAction = 'signup' | 'forgot' | 'resend-verification';

export type TurnstileResult =
  | { ok: true; skipped: boolean }
  | { ok: false; reason: 'missing-token' | 'rejected' | 'unavailable' };

export interface VerifyTurnstileInput {
  /** Value of the `cf-turnstile-response` form field. */
  token: FormDataEntryValue | null | undefined;
  /** Action the widget was rendered with; must echo back from Cloudflare. */
  action: TurnstileAction;
  /** Client IP (CF-Connecting-IP), optional. */
  remoteIp?: string | null;
  /** TURNSTILE_SECRET_KEY. Unset/empty disables verification. */
  secret?: string | null;
  /**
   * Hostname the challenge must have been solved on (host of NEXT_PUBLIC_SITE_URL).
   * Unset skips the hostname check (local dev).
   */
  expectedHostname?: string | null;
  fetchImpl?: typeof fetch;
  timeoutMs?: number;
}

/** True when the server should enforce Turnstile. */
export function turnstileEnabled(secret?: string | null): boolean {
  return typeof secret === 'string' && secret.trim().length > 0;
}

export async function verifyTurnstile(input: VerifyTurnstileInput): Promise<TurnstileResult> {
  if (!turnstileEnabled(input.secret)) return { ok: true, skipped: true };

  const { token } = input;
  if (typeof token !== 'string' || token.length === 0 || token.length > MAX_TOKEN_LENGTH) {
    return { ok: false, reason: 'missing-token' };
  }

  const body = new URLSearchParams({ secret: input.secret as string, response: token });
  if (input.remoteIp) body.set('remoteip', input.remoteIp);

  let data: { success?: boolean; hostname?: string; action?: string };
  try {
    const res = await (input.fetchImpl ?? fetch)(SITEVERIFY_URL, {
      method: 'POST',
      headers: { 'Content-Type': 'application/x-www-form-urlencoded' },
      body,
      signal: AbortSignal.timeout(input.timeoutMs ?? DEFAULT_TIMEOUT_MS),
    });
    if (!res.ok) return { ok: false, reason: 'unavailable' };
    data = await res.json();
  } catch {
    return { ok: false, reason: 'unavailable' };
  }

  if (data.success !== true) return { ok: false, reason: 'rejected' };
  if (data.action !== input.action) return { ok: false, reason: 'rejected' };
  if (input.expectedHostname && data.hostname !== input.expectedHostname) {
    return { ok: false, reason: 'rejected' };
  }
  return { ok: true, skipped: false };
}
