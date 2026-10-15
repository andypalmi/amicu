import Link from 'next/link';
import { Brand } from '@/components/common/brand';
import { Container } from './marketing';
import { ThemeToggle } from './theme-toggle';
import { MobileMenu, type MobileNavCta, type MobileNavItem } from './mobile-menu';
import { appHref } from '@/lib/shared';
import { SIGNIN_ENABLED } from '@/lib/features';
import { githubUrl, quickstartUrl } from '../_lib/content';

const siteLinks = [
  { href: '/docs', label: 'Docs' },
  { href: '/providers', label: 'Providers' },
  { href: '/pricing', label: 'Pricing' },
  { href: '/security', label: 'Security' },
];

const mobileItems: MobileNavItem[] = [
  {
    href: '/docs',
    label: 'Docs',
    children: [
      { href: '/docs/quickstart', label: 'Quickstart' },
      { href: '/docs/concepts', label: 'Concepts' },
      { href: '/docs/configuration', label: 'Configuration' },
      { href: '/docs/reference/cli', label: 'CLI reference' },
    ],
  },
  { href: '/providers', label: 'Providers' },
  { href: '/pricing', label: 'Pricing' },
  { href: '/security', label: 'Security' },
];

// Outline variants first, the filled primary last.
const mobileCtas: MobileNavCta[] = [
  { href: githubUrl, label: 'GitHub', variant: 'outline' },
  ...(SIGNIN_ENABLED
    ? ([
        { href: appHref('/login'), label: 'Sign in', variant: 'outline' },
        { href: appHref('/signup'), label: 'Get started', variant: 'default' },
      ] as MobileNavCta[])
    : ([{ href: quickstartUrl, label: 'Quickstart', variant: 'default' }] as MobileNavCta[])),
];

const linkCls =
  'rounded-sm text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring';

/**
 * Marketing header: the wordmark, site links (docs first), GitHub, a quiet sign-in link and the theme switch. Server-rendered;
 * below md the links fold into a full-screen client menu (mobile-menu.tsx).
 */
export function SiteHeader() {
  const all = siteLinks;
  return (
    <header className="sticky top-0 z-40 border-b border-border bg-background">
      <Container className="flex h-14 items-center gap-6">
        <Link
          href="/"
          aria-label="amicu home"
          className="rounded-sm text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          <Brand />
        </Link>

        <nav aria-label="Main" className="hidden items-center gap-6 text-sm md:flex">
          {all.map((l) => (
            <Link key={l.href} href={l.href} className={linkCls}>
              {l.label}
            </Link>
          ))}
          <a href={githubUrl} className={linkCls}>
            GitHub
          </a>
        </nav>

        <div className="ml-auto flex items-center gap-2 text-sm">
          {SIGNIN_ENABLED ? (
            <Link
              href={appHref('/login')}
              className={`${linkCls} hidden px-2 md:inline`}
            >
              Sign in
            </Link>
          ) : null}
          <ThemeToggle />
          <MobileMenu items={mobileItems} ctas={mobileCtas} />
        </div>
      </Container>
    </header>
  );
}
