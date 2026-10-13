// Same-origin guard for state-changing BFF requests (CSRF defence in depth on top
// of SameSite=Lax cookies). Pure and dependency-free so it runs under `node --test`
// and inside proxy.ts (Node middleware on OpenNext/Workers).
//
// Next.js server actions already enforce Origin vs Host/X-Forwarded-Host on their
// own (action-handler.js), so this guard is for route handlers under /api/*.

const SAFE_METHODS = new Set(['GET', 'HEAD', 'OPTIONS']);

function originOf(value: string | null | undefined): string | null {
  if (!value) return null;
  try {
    const u = new URL(value);
    return u.protocol === 'http:' || u.protocol === 'https:' ? u.origin : null;
  } catch {
    return null;
  }
}

export interface CsrfInput {
  method: string;
  /** `Origin` request header. */
  origin: string | null;
  /** `Sec-Fetch-Site` request header. */
  secFetchSite: string | null;
  /** `Host` header. */
  host: string | null;
  /** `X-Forwarded-Host` header (first value). */
  forwardedHost: string | null;
  /** `X-Forwarded-Proto` header (first value). */
  forwardedProto: string | null;
  /** Origin of the request URL as seen by the app. */
  requestOrigin: string;
  /** Configured public site URL (NEXT_PUBLIC_SITE_URL), if any. */
  siteUrl?: string | null;
  /** Configured app URL (NEXT_PUBLIC_APP_URL), if any - the app.amicu.dev host. */
  appUrl?: string | null;
}

/** The set of origins considered "ourselves" for this request. */
export function allowedOrigins(input: CsrfInput): Set<string> {
  const out = new Set<string>();
  const add = (o: string | null) => o && out.add(o);
  add(originOf(input.siteUrl));
  add(originOf(input.appUrl));
  add(originOf(input.requestOrigin));
  const proto = (input.forwardedProto?.split(',')[0]?.trim() || new URL(input.requestOrigin).protocol.replace(':', ''));
  for (const h of [input.forwardedHost, input.host]) {
    const host = h?.split(',')[0]?.trim();
    if (host) add(originOf(`${proto}://${host}`));
  }
  return out;
}

/**
 * True when the request may proceed. Safe methods always pass. Otherwise the
 * `Origin` header must match an allowed origin; if absent, `Sec-Fetch-Site` must be
 * `same-origin` (or `none`, user-initiated). Requests carrying neither header are
 * non-browser clients (no ambient cookies to ride) and pass.
 */
export function isSameOriginRequest(input: CsrfInput): boolean {
  if (SAFE_METHODS.has(input.method.toUpperCase())) return true;
  if (input.origin !== null) {
    const o = originOf(input.origin);
    return o !== null && allowedOrigins(input).has(o);
  }
  if (input.secFetchSite !== null) {
    return input.secFetchSite === 'same-origin' || input.secFetchSite === 'none';
  }
  return true;
}

/** Adapter for a Fetch `Request` (NextRequest included). */
export function requestIsSameOrigin(
  request: Request,
  siteUrl?: string | null,
  appUrl?: string | null,
): boolean {
  const h = request.headers;
  return isSameOriginRequest({
    method: request.method,
    origin: h.get('origin'),
    secFetchSite: h.get('sec-fetch-site'),
    host: h.get('host'),
    forwardedHost: h.get('x-forwarded-host'),
    forwardedProto: h.get('x-forwarded-proto'),
    requestOrigin: new URL(request.url).origin,
    siteUrl,
    appUrl,
  });
}
