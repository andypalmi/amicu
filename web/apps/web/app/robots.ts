import type { MetadataRoute } from 'next';
import { headers } from 'next/headers';
import { appUrl, siteUrl } from '@/lib/shared';
import { classifyHost } from '@/lib/routing/host';

// Marketing + docs host: allow the public surface; keep the authed product
// routes, the auth screens, and the BFF API out of crawlers (they are redirected
// to the app host anyway, and the paths stay listed for non-split deployments).
// App host (app.amicu.dev): the whole origin is private, so disallow everything.
export default async function robots(): Promise<MetadataRoute.Robots> {
  const h = await headers();
  const host = h.get('x-forwarded-host') ?? h.get('host');
  if (classifyHost(host, siteUrl, appUrl) === 'app') {
    return { rules: { userAgent: '*', disallow: '/' } };
  }
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
