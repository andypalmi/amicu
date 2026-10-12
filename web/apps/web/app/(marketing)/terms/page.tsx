import type { Metadata } from 'next';
import { company, legalPages } from '@/lib/legal/company';
import { pageMetadata } from '../_lib/seo';
import { Body, LegalPage, requireEnabled } from '../_components/legal';

export const metadata: Metadata = pageMetadata({
  title: 'Terms of Service',
  description: 'Terms governing use of Amicu.',
  path: '/terms',
});

export default function TermsPage() {
  requireEnabled(legalPages.terms);
  return (
    <LegalPage title="Terms of Service" effective={company.termsEffectiveDate}>
      {/* Owner/counsel-supplied terms (lib/legal/company.ts termsBody). */}
      <Body text={company.termsBody ?? ''} />
    </LegalPage>
  );
}
