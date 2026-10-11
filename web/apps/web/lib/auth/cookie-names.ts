// Session cookie names. Pure (no server-only) so it is unit-testable.
//
// When cookies are Secure (every non-dev environment) we use the `__Host-` prefix:
// browsers then only accept the cookie if it is Secure, has Path=/ and no Domain,
// which blocks subdomain cookie-tossing/fixation. Local http dev keeps plain names
// (a `__Host-` cookie cannot be set over http).

export interface CookieNames {
  access: string;
  refresh: string;
  /** Names used before the `__Host-` rename; never read, only cleared. */
  legacy: string[];
}

export function sessionCookieNames(secure: boolean): CookieNames {
  return secure
    ? { access: '__Host-amicu_at', refresh: '__Host-amicu_rt', legacy: ['amicu_at', 'amicu_rt'] }
    : { access: 'amicu_at', refresh: 'amicu_rt', legacy: [] };
}
