import type { Metadata } from 'next';
import { notFound } from 'next/navigation';
import { SIGNIN_ENABLED } from '@/lib/features';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { Badge } from '@amicu/ui';
import { TIER_LABELS } from '@amicu/auth';
import { appUrl, siteHref } from '@/lib/shared';
import { baseOptions } from '@/lib/layout.shared';
import { requireSession } from '@/lib/auth/session';
import { LegalLinks } from '@/components/common/legal-links';
import { SessionProvider } from '@/components/session-provider';

// The authed product surface is private: keep it out of search indexes.
export const metadata: Metadata = {
  robots: { index: false, follow: false },
};

// Authed product surface. The session is resolved (and silently refreshed by the
// BFF middleware in proxy.ts) before render; unauthenticated visitors are sent to
// /login. The verified claims are handed to client components via SessionProvider
// as plain data - the raw access/refresh tokens never cross into the browser.
export default async function Layout({ children }: LayoutProps<'/'>) {
  // Sign-in disabled: the console does not exist (proxy.ts 404s first; this is the backstop).
  if (!SIGNIN_ENABLED) notFound();
  const user = await requireSession();

  return (
    <SessionProvider user={user}>
      <HomeLayout
        {...baseOptions()}
        // `active: 'nested-url'` lets fumadocs mark a nav item active for its whole
        // section (exact URL or any nested route) using its own client-side
        // usePathname match. The active state is a colour shift to the accent-text
        // token (fd-primary) - never an edge bar.
        links={[
          // Prod serves the dashboard at the rewritten `/`, so match it exactly there.
          appUrl
            ? { text: 'Console', url: '/', active: 'url' as const }
            : { text: 'Console', url: '/app', active: 'nested-url' as const },
          { text: 'New config', url: '/new', active: 'nested-url' },
          { text: 'My Configs', url: '/configs', active: 'nested-url' },
          { text: 'Recommendations', url: '/recommend', active: 'nested-url' },
          { text: 'Monitoring', url: '/monitor', active: 'nested-url' },
          { text: 'Docs', url: siteHref('/docs'), active: 'none' },
          { text: 'Account', url: '/account', active: 'nested-url' },
          { text: 'Billing', url: '/billing', active: 'nested-url' },
          { text: 'Organization', url: '/org', active: 'nested-url' },
          // Display-only tier badge, sourced from the server-verified session (so it
          // never flashes a wrong/unknown value). `secondary: true` places it in the
          // header's right-hand group next to the account/theme controls. This is
          // presentation only - real tier gating is enforced by the API.
          {
            type: 'custom',
            secondary: true,
            children: (
              <Badge variant="secondary" title={`Plan: ${TIER_LABELS[user.tier]}`}>
                {TIER_LABELS[user.tier]}
              </Badge>
            ),
          },
        ]}
      >
        {children}
        <LegalLinks />
      </HomeLayout>
    </SessionProvider>
  );
}
