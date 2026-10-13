export const appName = 'amicu';
export const docsRoute = '/docs';
export const docsContentRoute = '/llms.mdx/docs';

// Canonical origin for the site, used by the Metadata API (metadataBase,
// canonical URLs, Open Graph), the sitemap, robots.txt, and llms.txt. Set
// NEXT_PUBLIC_SITE_URL in the deployment environment (no trailing slash). When
// unset, production builds fall back to https://amicu.dev and dev falls back to
// localhost so it works without configuration.
export const siteUrl =
  process.env.NEXT_PUBLIC_SITE_URL ||
  (process.env.NODE_ENV === 'production' ? 'https://amicu.dev' : 'http://localhost:3000');

// Origin of the product app (console + auth), served by the same Worker on its
// own host. Set NEXT_PUBLIC_APP_URL (no trailing slash). Production builds fall
// back to https://app.amicu.dev; in dev it is null, meaning "same origin as the
// site" (no host split, links stay relative).
export const appUrl: string | null =
  process.env.NEXT_PUBLIC_APP_URL ||
  (process.env.NODE_ENV === 'production' ? 'https://app.amicu.dev' : null);

/** Absolute link into the app from marketing/docs pages (relative in dev). */
export function appHref(path: string): string {
  return appUrl ? `${appUrl.replace(/\/+$/, '')}${path}` : path;
}

/** Link to the app dashboard: clean `https://app.amicu.dev/` in prod, `/app` in dev. */
export function appHomeHref(): string {
  return appUrl ? `${appUrl.replace(/\/+$/, '')}/` : '/app';
}

/** Link into the marketing/docs site from app pages (relative in dev). */
export function siteHref(path: string): string {
  return appUrl ? `${siteUrl.replace(/\/+$/, '')}${path}` : path;
}

/** One-line product summary reused across metadata and llms.txt. */
export const appTagline =
  'Open-source infrastructure as code for self-hosted LLMs: describe the model, hardware and runtime in HCL, check fit and data residency, then apply.';

export const gitConfig = {
  user: 'andypalmi',
  repo: 'amicu',
  branch: 'main',
};
