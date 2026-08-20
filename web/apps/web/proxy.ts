import { NextRequest, NextResponse } from 'next/server';
import { isMarkdownPreferred, rewritePath } from 'fumadocs-core/negotiation';
import { docsContentRoute, docsRoute } from '@/lib/shared';
import { attemptRefresh } from '@/lib/auth/middleware-refresh';
import { ACCESS_COOKIE, REFRESH_COOKIE, REFRESH_MAX_AGE, IS_PROD } from '@/lib/auth/config';

const { rewrite: rewriteDocs } = rewritePath(
  `${docsRoute}{/*path}`,
  `${docsContentRoute}{/*path}/content.md`,
);
const { rewrite: rewriteSuffix } = rewritePath(
  `${docsRoute}{/*path}.md`,
  `${docsContentRoute}{/*path}/content.md`,
);

// Routes whose access token the BFF proactively (silently) refreshes.
// Includes the /api/engine/* BFF routes so a session idle past the access-token
// TTL is rotated before the route handler runs (attemptRefresh writes the new
// cookie onto the forwarded request via NextResponse.next({ request }), so the
// same-request guardEngine() sees it) — no spurious 401 on Validate/Plan.
const PROTECTED_PREFIXES = ['/app', '/account', '/api/engine'];

// secure tied to NODE_ENV so local HTTP dev works; a non-prod build over plain
// HTTP would omit Secure. Real deployments run production mode behind TLS.
const cookieAttrs = {
  httpOnly: true,
  secure: IS_PROD,
  sameSite: 'lax',
  path: '/',
} as const;

async function handleAuth(request: NextRequest): Promise<NextResponse> {
  const outcome = await attemptRefresh(request);

  if (outcome.kind === 'rotated') {
    // Propagate the new access token to BOTH the downstream render (so the RSC
    // getSession() sees the fresh cookie this request) and the browser response.
    request.cookies.set(ACCESS_COOKIE, outcome.tokens.access_token);
    request.cookies.set(REFRESH_COOKIE, outcome.tokens.refresh_token);
    const res = NextResponse.next({ request });
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
    const res = NextResponse.next({ request });
    res.cookies.delete(ACCESS_COOKIE);
    res.cookies.delete(REFRESH_COOKIE);
    return res;
  }

  return NextResponse.next();
}

export default async function proxy(request: NextRequest) {
  const { pathname } = request.nextUrl;

  // Docs content negotiation (markdown rewrites) — unchanged.
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
  if (PROTECTED_PREFIXES.some((p) => pathname === p || pathname.startsWith(`${p}/`))) {
    return handleAuth(request);
  }

  return NextResponse.next();
}
