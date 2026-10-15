// Feature flags. Build-time: NEXT_PUBLIC_* values are inlined by `next build` /
// `cf:build`, so the same value is visible to server components, the proxy and
// client components. Unset, empty or anything other than exactly "true" => off.

/** Pure parser (unit-tested). */
export function parseSigninEnabled(value: string | undefined): boolean {
  return value === 'true';
}

/**
 * Whether sign-in is exposed. When false, every sign-in/sign-up link is hidden,
 * the auth pages (/login, /signup, /forgot, /verify) and the session-gated app
 * pages (/app, /account, /configs, ...) answer 404. /api routes are unchanged.
 * Literal `process.env.NEXT_PUBLIC_*` access is required for build-time inlining.
 */
export const SIGNIN_ENABLED = parseSigninEnabled(process.env.NEXT_PUBLIC_SIGNIN_ENABLED);
