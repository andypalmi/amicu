export const appName = 'Amicu';
export const docsRoute = '/docs';
export const docsContentRoute = '/llms.mdx/docs';

// Canonical origin for the site, used by the Metadata API (metadataBase,
// canonical URLs, Open Graph), the sitemap, robots.txt, and llms.txt. Set
// NEXT_PUBLIC_SITE_URL in the deployment environment (no trailing slash); the
// localhost fallback keeps dev and build working without configuration.
export const siteUrl = process.env.NEXT_PUBLIC_SITE_URL ?? 'http://localhost:3000';

/** One-line product summary reused across metadata and llms.txt. */
export const appTagline =
  'A thin, open-source client over the Amicu Rust Management API for designing, deploying, and operating EU-first LLM serving stacks.';

export const gitConfig = {
  user: 'andypalmi',
  repo: 'amicu',
  branch: 'main',
};
