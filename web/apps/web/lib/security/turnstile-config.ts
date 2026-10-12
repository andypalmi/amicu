// Public Turnstile site key (safe for the browser). Unset => widget not rendered.
// NEXT_PUBLIC_* is inlined at build time, so set it before `next build`/`cf:build`.
// The secret (TURNSTILE_SECRET_KEY) is server-only and read in app/(auth)/actions.ts.
export function turnstileSiteKey(): string | null {
  return process.env.NEXT_PUBLIC_TURNSTILE_SITE_KEY?.trim() || null;
}
