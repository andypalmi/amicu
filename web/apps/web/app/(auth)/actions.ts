'use server';

import { redirect } from 'next/navigation';
import {
  loginSchema,
  signupSchema,
  forgotSchema,
  fieldErrors,
  type FieldErrors,
} from '@amicu/auth';
import { authClient } from '@/lib/auth/client';
import {
  setSessionCookies,
  clearSessionCookies,
  readRefreshToken,
} from '@/lib/auth/cookies';

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

function safeNext(raw: FormDataEntryValue | null): string {
  // Only allow local, absolute in-app paths — never an open redirect.
  const value = typeof raw === 'string' ? raw : '';
  return value.startsWith('/') && !value.startsWith('//') ? value : '/app';
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
  const parsed = signupSchema.safeParse({
    email: formData.get('email'),
    password: formData.get('password'),
  });
  if (!parsed.success) {
    return { fieldErrors: fieldErrors(parsed.error) };
  }

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

  // Fire-and-forget from the UI's perspective: the outcome is intentionally opaque.
  // We always return the same confirmation so an attacker cannot probe which emails
  // are registered.
  await authClient.requestPasswordReset(parsed.data);
  return {
    notice: 'If an account exists for that address, a reset link is on its way.',
  };
}

export async function logoutAction(): Promise<void> {
  // Revoke the refresh token server-side, then clear both cookies. Revocation is
  // best-effort — the cookies are cleared regardless so the browser session ends.
  const refresh = await readRefreshToken();
  if (refresh) {
    await authClient.logout(refresh);
  }
  await clearSessionCookies();
  redirect('/login');
}
