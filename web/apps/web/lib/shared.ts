export const appName = 'Amicu';
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

/** One-line product summary reused across metadata and llms.txt. */
export const appTagline =
  'Open-source infrastructure as code for self-hosted LLMs: describe the model, hardware and runtime in HCL, check fit and data residency, then apply.';

export const gitConfig = {
  user: 'andypalmi',
  repo: 'amicu',
  branch: 'main',
};
