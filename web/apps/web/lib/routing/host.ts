// Host-based routing for the two-host deployment: ONE Worker serves both the
// marketing/docs site (https://amicu.dev) and the product app
// (https://app.amicu.dev). Pure and dependency-free so it is unit-testable under
// `node --test` and callable from proxy.ts.
//
// The split only activates when the request host equals one of the two
// configured hosts. Anywhere else (localhost, previews, *.workers.dev) nothing is
// redirected, so local dev keeps serving every route on a single origin.

export type HostAction =
  | { kind: 'next' }
  /** Internal rewrite (URL in the browser does not change). */
  | { kind: 'rewrite'; pathname: string }
  | { kind: 'redirect'; url: string; status: 301 | 307 };

/** Top-level path prefixes served by the (app) and (auth) route groups. */
export const APP_PATH_PREFIXES = [
  '/app',
  '/account',
  '/billing',
  '/configs',
  '/monitor',
  '/new',
  '/org',
  '/recommend',
  '/wizard',
  '/login',
  '/signup',
  '/forgot',
  '/verify',
] as const;

/** Marketing / docs / discovery paths that live only on the site host. */
export const SITE_PATH_PREFIXES = [
  '/pricing',
  '/providers',
  '/security',
  '/privacy',
  '/terms',
  '/imprint',
  '/docs',
  '/llms.txt',
  '/llms-full.txt',
  '/llms.mdx',
  '/sitemap.xml',
] as const;

function matchesPrefix(pathname: string, prefix: string): boolean {
  return pathname === prefix || pathname.startsWith(`${prefix}/`);
}

export const isAppPath = (pathname: string): boolean =>
  APP_PATH_PREFIXES.some((p) => matchesPrefix(pathname, p));

export const isSitePath = (pathname: string): boolean =>
  pathname === '/' || SITE_PATH_PREFIXES.some((p) => matchesPrefix(pathname, p));

/** Lowercase, drop a trailing dot on the hostname (`amicu.dev.` === `amicu.dev`). */
function normalizeHost(host: string): string {
  const m = /^(.*?)(:\d+)?$/.exec(host.toLowerCase());
  const name = (m?.[1] ?? host).replace(/\.+$/, '');
  return `${name}${m?.[2] ?? ''}`;
}

function hostOf(origin: string | null | undefined): string | null {
  if (!origin) return null;
  try {
    return normalizeHost(new URL(origin).host);
  } catch {
    return null;
  }
}

const stripPort = (host: string): string => host.replace(/:\d+$/, '');

/** Percent-decode a pathname; null when malformed (never throws). */
function safeDecode(pathname: string): string | null {
  try {
    return decodeURIComponent(pathname);
  } catch {
    return null;
  }
}

export type HostKind = 'app' | 'site' | 'other';

/** Classify a request host against the configured site/app origins. */
export function classifyHost(
  host: string | null | undefined,
  siteOrigin: string | null | undefined,
  appOrigin: string | null | undefined,
): HostKind {
  const raw = host?.split(',')[0]?.trim();
  const h = raw ? normalizeHost(raw) : null;
  const siteHost = hostOf(siteOrigin);
  const appHost = hostOf(appOrigin);
  // Both must be configured and distinct, otherwise there is no split.
  if (!h || !siteHost || !appHost || siteHost === appHost) return 'other';
  // A port is compared only when the configured origin has one (local setups);
  // otherwise `amicu.dev:443` style hosts still classify.
  const eq = (cfg: string) => (/:\d+$/.test(cfg) ? h === cfg : stripPort(h) === cfg);
  if (eq(appHost)) return 'app';
  if (eq(siteHost)) return 'site';
  return 'other';
}

export interface HostRouteInput {
  host: string | null | undefined;
  pathname: string;
  /** Query string including the leading `?`, or ''. */
  search?: string;
  siteOrigin: string | null | undefined;
  appOrigin: string | null | undefined;
}

/** Decide what to do with a (host, path) pair. /api/* is never touched. */
export function resolveHostRoute(input: HostRouteInput): HostAction {
  const { pathname: rawPath } = input;
  const search = input.search ?? '';
  const kind = classifyHost(input.host, input.siteOrigin, input.appOrigin);
  if (kind === 'other') return { kind: 'next' };
  // Match on the decoded path (so `/%61pp` cannot dodge the split); redirect with
  // the original. A malformed escape matches nothing and falls through.
  const decoded = safeDecode(rawPath);
  if (decoded === null) return { kind: 'next' };
  const pathname = decoded.length > 1 ? decoded.replace(/\/+$/, '') || '/' : decoded;
  if (pathname === '/api' || pathname.startsWith('/api/')) return { kind: 'next' };

  const trim = (o: string) => o.replace(/\/+$/, '');
  const app = trim(input.appOrigin!);
  const site = trim(input.siteOrigin!);

  if (kind === 'app') {
    // Clean home: app.amicu.dev/ is the dashboard, /app is its alias.
    if (pathname === '/') return { kind: 'rewrite', pathname: '/app' };
    if (pathname === '/app') return { kind: 'redirect', url: `${app}/${search}`, status: 307 };
    if (isSitePath(pathname)) {
      return { kind: 'redirect', url: `${site}${rawPath}${search}`, status: 301 };
    }
    return { kind: 'next' };
  }

  // kind === 'site'
  if (pathname === '/app') return { kind: 'redirect', url: `${app}/${search}`, status: 301 };
  if (isAppPath(pathname)) {
    return { kind: 'redirect', url: `${app}${rawPath}${search}`, status: 301 };
  }
  return { kind: 'next' };
}
