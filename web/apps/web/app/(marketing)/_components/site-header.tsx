import Link from 'next/link';
import { Container } from './marketing';
import { ThemeToggle } from './theme-toggle';
import { githubUrl } from '../_lib/content';

const productLinks = [
  { href: '/#how-it-works', label: 'How it works' },
  { href: '/#where-it-runs', label: 'Where it runs' },
];

const siteLinks = [
  { href: '/pricing', label: 'Pricing' },
  { href: '/security', label: 'Security' },
  { href: '/docs', label: 'Docs' },
];

const linkCls =
  'rounded-sm text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring';

/**
 * Marketing header: a typeset wordmark (there is no logo), product anchors,
 * secondary site links, GitHub, sign-in and the theme switch. Server-rendered;
 * on narrow screens the links fold into a native <details> menu (no JS).
 */
export function SiteHeader() {
  const all = [...productLinks, ...siteLinks];
  return (
    <header className="sticky top-0 z-40 border-b border-border bg-background/90 backdrop-blur supports-[backdrop-filter]:bg-background/75">
      <Container className="flex h-14 items-center gap-6">
        <Link
          href="/"
          className="rounded-sm text-xl font-semibold tracking-tight text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          amicu
        </Link>

        <nav aria-label="Main" className="hidden items-center gap-6 text-sm lg:flex">
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
          <Link
            href="/login"
            className="rounded-md px-3 py-1.5 font-medium text-foreground transition-colors hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
          >
            Sign in
          </Link>
          <ThemeToggle />
          <details className="group relative lg:hidden">
            <summary
              className="flex size-9 cursor-pointer list-none items-center justify-center rounded-md text-muted-foreground hover:bg-accent hover:text-foreground [&::-webkit-details-marker]:hidden"
              aria-label="Menu"
            >
              <span aria-hidden className="flex w-4 flex-col gap-1">
                <span className="h-px w-full bg-current" />
                <span className="h-px w-full bg-current" />
                <span className="h-px w-full bg-current" />
              </span>
            </summary>
            <nav
              aria-label="Main"
              className="absolute right-0 top-11 flex w-56 flex-col rounded-md border border-border bg-popover p-2 text-popover-foreground"
            >
              {all.map((l) => (
                <Link
                  key={l.href}
                  href={l.href}
                  className="rounded-md px-3 py-2 text-muted-foreground hover:bg-accent hover:text-foreground"
                >
                  {l.label}
                </Link>
              ))}
              <a
                href={githubUrl}
                className="rounded-md px-3 py-2 text-muted-foreground hover:bg-accent hover:text-foreground"
              >
                GitHub
              </a>
            </nav>
          </details>
        </div>
      </Container>
    </header>
  );
}
