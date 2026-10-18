import Link from 'next/link';
import { company, legalPages } from '@/lib/legal/company';
import { githubUrl } from '../_lib/content';
import { Brand } from '@/components/common/brand';
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
      <Container className="flex flex-col items-center gap-6 text-center">
        <div className="flex flex-col items-center gap-1">
          <Brand className="text-lg" />
          <span className="text-muted-foreground">Open source under AGPL-3.0.</span>
        </div>
        <div className="flex flex-col items-center gap-3">
          <nav aria-label="Footer" className="flex flex-wrap justify-center gap-x-6 gap-y-2">
            <Link className={linkCls} href="/install">Install</Link>
            <Link className={linkCls} href="/docs/quickstart">Quickstart</Link>
            <Link className={linkCls} href="/docs">Docs</Link>
            <Link className={linkCls} href="/pricing">Pricing</Link>
            <Link className={linkCls} href="/providers">Providers</Link>
            <Link className={linkCls} href="/security">Security</Link>
            <a className={linkCls} href={githubUrl}>GitHub</a>
            <a className={linkCls} href={`${githubUrl}/blob/main/LICENSE`}>License</a>
          </nav>
          {legal.length > 0 || legalPages.contact ? (
            <nav aria-label="Legal" className="flex flex-wrap justify-center gap-x-6 gap-y-2">
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
