import Link from 'next/link';
import { company, legalPages } from '@/lib/legal/company';
import { siteHref } from '@/lib/shared';

/** Privacy / Terms / Contact links for the app and auth shells (marketing has its own footer). */
export function LegalLinks() {
  if (!legalPages.privacy && !legalPages.terms && !legalPages.contact) return null;
  const cls = 'text-muted-foreground hover:text-foreground hover:underline';
  return (
    <nav
      aria-label="Legal"
      className="flex flex-wrap justify-center gap-x-6 gap-y-2 border-t border-border px-6 py-6 text-sm"
    >
      {legalPages.privacy ? (
        <Link className={cls} href={siteHref('/privacy')}>
          Privacy
        </Link>
      ) : null}
      {legalPages.terms ? (
        <Link className={cls} href={siteHref('/terms')}>
          Terms
        </Link>
      ) : null}
      {legalPages.contact ? (
        <a className={cls} href={`mailto:${company.contactEmail}`}>
          Contact
        </a>
      ) : null}
    </nav>
  );
}
