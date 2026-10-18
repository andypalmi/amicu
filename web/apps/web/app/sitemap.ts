import type { MetadataRoute } from 'next';
import { source } from '@/lib/source';
import { legalPages } from '@/lib/legal/company';
import { siteUrl } from '@/lib/shared';

// Public sitemap: the marketing routes plus every generated docs page. Docs
// entries are derived from the fumadocs page source map (not hardcoded), so new
// content under content/docs shows up automatically. Private areas ((app) and
// (auth)) are intentionally excluded and additionally carry robots noindex.

const marketingRoutes = [
  '/',
  '/install',
  '/pricing',
  '/providers',
  '/security',
  // Legal pages are listed only per lib/legal/company.ts (imprint stays hidden until identity is filled).
  ...(legalPages.privacy ? ['/privacy'] : []),
  ...(legalPages.terms ? ['/terms'] : []),
  ...(legalPages.imprint ? ['/imprint'] : []),
];

function absolute(path: string): string {
  return new URL(path, siteUrl).toString();
}

export default function sitemap(): MetadataRoute.Sitemap {
  const lastModified = new Date();

  const marketing: MetadataRoute.Sitemap = marketingRoutes.map((path) => ({
    url: absolute(path),
    lastModified,
    changeFrequency: 'monthly',
    priority: path === '/' ? 1 : 0.8,
  }));

  const docs: MetadataRoute.Sitemap = source.getPages().map((page) => ({
    url: absolute(page.url),
    lastModified,
    changeFrequency: 'weekly',
    priority: 0.6,
  }));

  return [...marketing, ...docs];
}
