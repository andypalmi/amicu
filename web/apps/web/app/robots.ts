import type { MetadataRoute } from 'next';
import { siteUrl } from '@/lib/shared';

// Allow the public marketing + docs surface; keep the authed product routes,
// the auth screens, and the BFF API out of crawlers. These paths mirror the
// (app) and (auth) route groups (which live at the site root) plus /api.
export default function robots(): MetadataRoute.Robots {
  return {
    rules: {
      userAgent: '*',
      allow: '/',
      disallow: [
        '/api/',
        '/app',
        '/account',
        '/billing',
        '/monitor',
        '/org',
        '/recommend',
        '/login',
        '/signup',
        '/forgot',
      ],
    },
    sitemap: new URL('/sitemap.xml', siteUrl).toString(),
    host: siteUrl,
  };
}
