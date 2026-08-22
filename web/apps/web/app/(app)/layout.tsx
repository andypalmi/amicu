import type { Metadata } from 'next';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { Badge } from '@amicu/ui';
import { TIER_LABELS } from '@amicu/auth';
import { baseOptions } from '@/lib/layout.shared';
import { requireSession } from '@/lib/auth/session';
import { SessionProvider } from '@/components/session-provider';

// The authed product surface is private: keep it out of search indexes.
export const metadata: Metadata = {
  robots: { index: false, follow: false },
};

// Authed product surface. The session is resolved (and silently refreshed by the
// BFF middleware in proxy.ts) before render; unauthenticated visitors are sent to
// /login. The verified claims are handed to client components via SessionProvider
// as plain data — the raw access/refresh tokens never cross into the browser.
export default async function Layout({ children }: LayoutProps<'/'>) {
  const user = await requireSession();

  return (
    <SessionProvider user={user}>
      <HomeLayout
        {...baseOptions()}
        // `active: 'nested-url'` lets fumadocs mark a nav item active for its whole
        // section (exact URL or any nested route) using its own client-side
        // usePathname match. In the neutral theme the active state is a neutral
        // foreground shift (muted-foreground -> fd-primary, hsl 0 0% 9%) — no accent.
        links={[
          { text: 'Console', url: '/app', active: 'nested-url' },
          { text: 'Recommendations', url: '/recommend', active: 'nested-url' },
          { text: 'Monitoring', url: '/monitor', active: 'nested-url' },
          { text: 'Docs', url: '/docs', active: 'nested-url' },
          { text: 'Account', url: '/account', active: 'nested-url' },
          { text: 'Billing', url: '/billing', active: 'nested-url' },
          { text: 'Organization', url: '/org', active: 'nested-url' },
          // Display-only tier badge, sourced from the server-verified session (so it
          // never flashes a wrong/unknown value). `secondary: true` places it in the
          // header's right-hand group next to the account/theme controls. This is
          // presentation only — real tier gating is enforced by the API.
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
      </HomeLayout>
    </SessionProvider>
  );
}
