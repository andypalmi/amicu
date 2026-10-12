import type { Viewport } from 'next';
import Link from 'next/link';
import { HomeLayout } from 'fumadocs-ui/layouts/home';
import { Button } from '@amicu/ui';
import { SiteFooter } from './_components/footer';
import { baseOptions } from '@/lib/layout.shared';

// viewport-fit=cover lets env(safe-area-inset-*) report real insets on notched
// phones (used by the mobile CTA bar). Scoped to marketing only.
export const viewport: Viewport = { viewportFit: 'cover' };

export default function Layout({ children }: LayoutProps<'/'>) {
  return (
    <HomeLayout
      {...baseOptions()}
      links={[
        { text: 'Pricing', url: '/pricing' },
        { text: 'Providers', url: '/providers' },
        { text: 'Security', url: '/security' },
        { text: 'Docs', url: '/docs' },
        { text: 'App', url: '/app' },
      ]}
    >
      {/* Bottom padding keeps the fixed mobile CTA bar from covering the footer. */}
      <div className="flex flex-1 flex-col pb-[calc(4.5rem+env(safe-area-inset-bottom))] md:pb-0">
        {children}
        <SiteFooter />
      </div>
      <MobileCtaBar />
    </HomeLayout>
  );
}

// Sticky bottom CTA, phones only (the desktop nav already carries "App").
function MobileCtaBar() {
  return (
    <div className="fixed inset-x-0 bottom-0 z-40 border-t border-border bg-background/95 px-4 pt-3 pb-[calc(0.75rem+env(safe-area-inset-bottom))] backdrop-blur md:hidden">
      <div className="mx-auto flex max-w-md gap-3">
        <Button asChild variant="outline" className="flex-1">
          <Link href="/docs">Read the docs</Link>
        </Button>
        <Button asChild className="flex-1">
          <Link href="/app">Open the console</Link>
        </Button>
      </div>
    </div>
  );
}
