import Link from 'next/link';
import { company, legalPages } from '@/lib/legal/company';
import { githubUrl } from '../_lib/content';
import { Container } from './marketing';

const linkCls = 'text-muted-foreground hover:text-foreground hover:underline';

export function SiteFooter() {
  const legal = [
    legalPages.privacy && { href: '/privacy', label: 'Privacy' },
    legalPages.terms && { href: '/terms', label: 'Terms' },
    legalPages.imprint && { href: '/imprint', label: 'Imprint' },
  ].filter(Boolean) as { href: string; label: string }[];

  return (
    <footer className="border-t border-border py-10 text-sm">
      <Container className="flex flex-col gap-6 sm:flex-row sm:items-start sm:justify-between">
        <div className="flex flex-col gap-1">
          <span className="font-semibold tracking-tight text-foreground">amicu</span>
          <span className="text-muted-foreground">Open source under AGPL-3.0.</span>
        </div>
        <div className="flex flex-col gap-3 sm:items-end">
          <nav aria-label="Footer" className="flex flex-wrap gap-x-6 gap-y-2">
            <Link className={linkCls} href="/pricing">Pricing</Link>
            <Link className={linkCls} href="/providers">Providers</Link>
            <Link className={linkCls} href="/security">Security</Link>
            <Link className={linkCls} href="/docs">Docs</Link>
            <a className={linkCls} href={githubUrl}>GitHub</a>
          </nav>
          {legal.length > 0 || legalPages.contact ? (
            <nav aria-label="Legal" className="flex flex-wrap gap-x-6 gap-y-2">
              {legal.map((l) => (
                <Link key={l.href} className={linkCls} href={l.href}>{l.label}</Link>
              ))}
              {legalPages.contact ? (
                <a className={linkCls} href={`mailto:${company.contactEmail}`}>Contact</a>
              ) : null}
            </nav>
          ) : null}
        </div>
      </Container>
    </footer>
  );
}
