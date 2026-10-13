import { NextRequest, NextResponse } from 'next/server';
import { isMarkdownPreferred, rewritePath } from 'fumadocs-core/negotiation';
import { appUrl, docsContentRoute, docsRoute, siteUrl } from '@/lib/shared';
import { resolveHostRoute } from '@/lib/routing/host';
import { attemptRefresh } from '@/lib/auth/middleware-refresh';
import {
  ACCESS_COOKIE,
  REFRESH_COOKIE,
  REFRESH_MAX_AGE,
  COOKIE_SECURE,
  LEGACY_COOKIES,
  SITE_URL,
  APP_URL,
} from '@/lib/auth/config';
import { requestIsSameOrigin } from '@/lib/security/csrf';

const { rewrite: rewriteDocs } = rewritePath(
  `${docsRoute}{/*path}`,
  `${docsContentRoute}{/*path}/content.md`,
);
const { rewrite: rewriteSuffix } = rewritePath(
  `${docsRoute}{/*path}.md`,
  `${docsContentRoute}{/*path}/content.md`,
);

// Prefixes that trigger SILENT TOKEN REFRESH ONLY - this is NOT an access-control
// gate. The middleware never blocks or redirects unauthenticated requests; it
// merely rotates a soon-to-expire access token so authed surfaces don't 401 mid
// session. Real authorization lives in requireSession() (each (app) server
// component) and guardEngine() (each BFF route). Includes the /api/engine/* BFF
// routes so a session idle past the access-token TTL is rotated before the route
// handler runs (attemptRefresh writes the new cookie onto the forwarded request
// via NextResponse.next({ request }), so the same-request guardEngine() sees it)
// - no spurious 401 on Validate/Plan.
const REFRESH_PREFIXES = [
  '/app',
  '/account',
  '/api/engine',
  '/api/recommend',
  '/recommend',
  '/api/monitor',
  '/monitor',
  '/billing',
  '/api/billing',
  '/org',
  '/api/org',
];

// secure is explicit (COOKIE_SECURE, see server-env.ts) so local HTTP dev works
// while every other environment defaults to Secure. Real deployments run behind
// TLS regardless.
const cookieAttrs = {
  httpOnly: true,
  secure: COOKIE_SECURE,
  sameSite: 'lax',
  path: '/',
} as const;

// Wraps the "continue" response so a host-level rewrite (app.amicu.dev/ -> /app)
// composes with silent refresh. `rewriteTo` null => plain pass-through.
function proceed(request: NextRequest, rewriteTo: string | null): NextResponse {
  if (!rewriteTo) return NextResponse.next({ request });
  const url = request.nextUrl.clone();
  url.pathname = rewriteTo;
  return NextResponse.rewrite(url, { request });
}

async function handleAuth(request: NextRequest, rewriteTo: string | null): Promise<NextResponse> {
  const outcome = await attemptRefresh(request);

  if (outcome.kind === 'rotated') {
    // Propagate the new access token to BOTH the downstream render (so the RSC
    // getSession() sees the fresh cookie this request) and the browser response.
    request.cookies.set(ACCESS_COOKIE, outcome.tokens.access_token);
    request.cookies.set(REFRESH_COOKIE, outcome.tokens.refresh_token);
    const res = proceed(request, rewriteTo);
    res.cookies.set(ACCESS_COOKIE, outcome.tokens.access_token, {
      ...cookieAttrs,
      maxAge: outcome.tokens.expires_in ?? 60 * 15,
    });
    res.cookies.set(REFRESH_COOKIE, outcome.tokens.refresh_token, {
      ...cookieAttrs,
      maxAge: REFRESH_MAX_AGE,
    });
    return res;
  }

  if (outcome.kind === 'revoked') {
    request.cookies.delete(ACCESS_COOKIE);
    request.cookies.delete(REFRESH_COOKIE);
    const res = proceed(request, rewriteTo);
    // Expire with Secure+Path=/ (a __Host- cookie can't be deleted without them).
    for (const name of [ACCESS_COOKIE, REFRESH_COOKIE, ...LEGACY_COOKIES]) {
      res.cookies.set(name, '', { ...cookieAttrs, maxAge: 0 });
    }
    return res;
  }

  return proceed(request, rewriteTo);
}

export default async function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;

  // CSRF: state-changing BFF route-handler requests must be same-origin. (Server
  // actions get Next's own Origin/Host check; this covers /api/*.)
  if (
    (pathname === '/api' || pathname.startsWith('/api/')) &&
    !requestIsSameOrigin(request, SITE_URL, APP_URL)
  ) {
    return NextResponse.json({ error: 'cross-origin request rejected' }, { status: 403 });
  }

  // Host split (amicu.dev marketing/docs vs app.amicu.dev console + auth). Inert
  // unless the request host is one of the two configured hosts (so localhost and
  // previews serve every route). /api/* is never redirected.
  const host =
    request.headers.get('x-forwarded-host') ?? request.headers.get('host') ?? request.nextUrl.host;
  const hostAction = resolveHostRoute({
    host,
    pathname,
    search: request.nextUrl.search,
    siteOrigin: siteUrl,
    appOrigin: appUrl,
  });
  if (hostAction.kind === 'redirect') {
    return NextResponse.redirect(hostAction.url, hostAction.status === 301 ? 301 : 307);
  }
  const rewriteTo = hostAction.kind === 'rewrite' ? hostAction.pathname : null;

  // Docs content negotiation (markdown rewrites) - unchanged.
  const suffix = rewriteSuffix(pathname);
  if (suffix) {
    return NextResponse.rewrite(new URL(suffix, request.nextUrl));
  }

  if (isMarkdownPreferred(request)) {
    const docs = rewriteDocs(pathname);
    if (docs) {
      return NextResponse.rewrite(new URL(docs, request.nextUrl), {
        headers: { Vary: 'Accept' },
      });
    }
  }

  // Silent session refresh for authed surfaces.
  const effectivePath = rewriteTo ?? pathname;
  if (REFRESH_PREFIXES.some((p) => effectivePath === p || effectivePath.startsWith(`${p}/`))) {
    return handleAuth(request, rewriteTo);
  }

  return rewriteTo ? proceed(request, rewriteTo) : NextResponse.next();
}
