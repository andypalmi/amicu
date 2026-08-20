import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { baseOptions } from '@/lib/layout.shared';
import { requireSession } from '@/lib/auth/session';
import { SessionProvider } from '@/components/session-provider';

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
        links={[
          { text: 'Docs', url: '/docs' },
          { text: 'Account', url: '/account' },
        ]}
      >
        {children}
      </HomeLayout>
    </SessionProvider>
  );
}
