'use server';

import { headers } from 'next/headers';
import { redirect } from 'next/navigation';
import {
  loginSchema,
  signupSchema,
  forgotSchema,
  fieldErrors,
  type FieldErrors,
} from '@amicu/auth';
import { appHomeHref } from '@/lib/shared';
import { safeNextPath } from '@/lib/auth/safe-next';
import { authClient } from '@/lib/auth/client';
import { SIGNUPS_ENABLED } from '@/lib/auth/config';
import { ACCEPT_TERMS_ERROR, ACCEPT_TERMS_FIELD, termsAccepted } from '@/lib/auth/consent';
import {
  setSessionCookies,
  clearSessionCookies,
  readRefreshToken,
} from '@/lib/auth/cookies';
import {
  TURNSTILE_RESPONSE_FIELD,
  verifyTurnstile,
  type TurnstileAction,
} from '@/lib/security/turnstile';

// Result surfaced back to the client forms via useActionState.
export interface AuthFormState {
  error?: string;
  fieldErrors?: FieldErrors;
  /** Generic confirmation message (e.g. password reset), not tied to any field. */
  notice?: string;
}

// Anti-enumeration: authentication failures always read the same, regardless of
// whether the email exists or the password was wrong (RFC 0001 §3, TASK-078).
const GENERIC_CREDS = 'Invalid email or password.';
const GENERIC_UNAVAILABLE = 'Something went wrong. Please try again.';

// Same message for every Turnstile failure (missing token, rejected, Cloudflare
// outage - fail-closed, see lib/security/turnstile.ts). It never depends on the
// email, so it leaks nothing about account existence.
const GENERIC_CHALLENGE = 'Verification failed. Please refresh the page and try again.';

// Returns an error state when the Turnstile check fails; null when it passes or is
// disabled (no TURNSTILE_SECRET_KEY). Runs AFTER input validation (so typos don't
// burn a single-use token) and BEFORE any call to the API.
async function challengeFailure(
  formData: FormData,
  action: TurnstileAction,
): Promise<AuthFormState | null> {
  // Auth forms live on the app host, so the widget is solved there.
  const siteUrl = process.env.NEXT_PUBLIC_APP_URL || process.env.NEXT_PUBLIC_SITE_URL;
  let expectedHostname: string | null = null;
  try {
    expectedHostname = siteUrl ? new URL(siteUrl).hostname : null;
  } catch {
    expectedHostname = null;
  }
  const result = await verifyTurnstile({
    token: formData.get(TURNSTILE_RESPONSE_FIELD),
    action,
    remoteIp: (await headers()).get('cf-connecting-ip'),
    secret: process.env.TURNSTILE_SECRET_KEY,
    expectedHostname,
  });
  return result.ok ? null : { error: GENERIC_CHALLENGE };
}

function safeNext(raw: FormDataEntryValue | null): string {
  return safeNextPath(raw, appHomeHref());
}

export async function loginAction(
  _prev: AuthFormState,
  formData: FormData,
): Promise<AuthFormState> {
  const parsed = loginSchema.safeParse({
    email: formData.get('email'),
    password: formData.get('password'),
  });
  if (!parsed.success) {
    return { fieldErrors: fieldErrors(parsed.error) };
  }

  const result = await authClient.login(parsed.data);
  if (!result.ok) {
    // 5xx/network → generic "try again"; everything else → generic credential error.
    return { error: result.status >= 500 ? GENERIC_UNAVAILABLE : GENERIC_CREDS };
  }

  await setSessionCookies(result.data);
  redirect(safeNext(formData.get('next')));
}

export async function signupAction(
  _prev: AuthFormState,
  formData: FormData,
): Promise<AuthFormState> {
  // Closed sign-ups: refuse before touching the API (or the challenge).
  if (!SIGNUPS_ENABLED) {
    return { error: 'Sign-ups are currently closed.' };
  }
  const parsed = signupSchema.safeParse({
    email: formData.get('email'),
    password: formData.get('password'),
  });
  const errors: FieldErrors = parsed.success ? {} : fieldErrors(parsed.error);
  // Age + terms consent is enforced server-side too (the client check is only UX).
  if (!termsAccepted(formData.get(ACCEPT_TERMS_FIELD))) errors[ACCEPT_TERMS_FIELD] = ACCEPT_TERMS_ERROR;
  if (!parsed.success || Object.keys(errors).length > 0) {
    return { fieldErrors: errors };
  }

  const blocked = await challengeFailure(formData, 'signup');
  if (blocked) return blocked;

  const result = await authClient.signup(parsed.data);
  if (!result.ok) {
    return { error: GENERIC_UNAVAILABLE };
  }

  // The backend returns 202 with no tokens and never reveals whether the email
  // was already registered (anti-enumeration). We therefore do NOT log the user
  // in here; we show the same generic acknowledgement in every case. Confirmation
  // happens out-of-band (email), after which the user signs in.
  return {
    notice: 'Check your inbox to confirm your account, then sign in.',
  };
}

export async function forgotAction(
  _prev: AuthFormState,
  formData: FormData,
): Promise<AuthFormState> {
  const parsed = forgotSchema.safeParse({ email: formData.get('email') });
  if (!parsed.success) {
    return { fieldErrors: fieldErrors(parsed.error) };
  }

  const blocked = await challengeFailure(formData, 'forgot');
  if (blocked) return blocked;

  // Fire-and-forget from the UI's perspective: the outcome is intentionally opaque.
  // We always return the same confirmation so an attacker cannot probe which emails
  // are registered.
  await authClient.requestPasswordReset(parsed.data);
  return {
    notice: 'If an account exists for that address, a reset link is on its way.',
  };
}

export async function resendVerificationAction(
  _prev: AuthFormState,
  formData: FormData,
): Promise<AuthFormState> {
  // The verification link carries only an email; reuse the forgot-flow schema
  // (email-only, same shape).
  const parsed = forgotSchema.safeParse({ email: formData.get('email') });
  if (!parsed.success) {
    return { fieldErrors: fieldErrors(parsed.error) };
  }

  const blocked = await challengeFailure(formData, 'resend-verification');
  if (blocked) return blocked;

  // Fire-and-forget from the UI's perspective: the outcome is intentionally
  // opaque. We always return the same confirmation so an attacker cannot probe
  // which emails have an unverified account. The backend likewise responds `202`
  // whether the account is unknown, already verified, or freshly re-linked.
  await authClient.resendVerification(parsed.data);
  return {
    notice:
      'If an unverified account exists for that address, a new link is on its way.',
  };
}

// Outcome of an email-verification attempt, surfaced to the /verify page.
//   - 'success': the token was consumed and the email is now confirmed.
//   - 'invalid': the backend rejected the token (unknown/expired/already used) with
//     the generic `400` - indistinguishable by design (anti-enumeration).
//   - 'error':  transient failure (rate limit / 5xx / network) - worth a retry.
export type VerifyStatus = 'success' | 'invalid' | 'error';

// Confirm an email address from the verification link's token. Called on load by
// the /verify page (a GET landing), not from a form. The token is opaque and may
// be absent or garbage; callers guard the empty case before invoking this.
export async function verifyAction(token: string): Promise<VerifyStatus> {
  if (!token) return 'invalid';

  const result = await authClient.verify({ token });
  if (result.ok) return 'success';

  // 400 is the only "this link is bad" signal; treat rate-limit/5xx/network as a
  // transient error the user can retry, not a permanently invalid link.
  return result.status === 400 ? 'invalid' : 'error';
}

export async function logoutAction(): Promise<void> {
  // Revoke the refresh token server-side, then clear both cookies. Revocation is
  // best-effort - the cookies are cleared regardless so the browser session ends.
  const refresh = await readRefreshToken();
  if (refresh) {
    await authClient.logout(refresh);
  }
  await clearSessionCookies();
  redirect('/login');
}
