import type { Metadata } from 'next';
import { company, legalPages } from '@/lib/legal/company';
import { pageMetadata } from '../_lib/seo';
import { LegalPage, requireEnabled } from '../_components/legal';

export const metadata: Metadata = pageMetadata({
  title: 'Imprint',
  description: 'Legal notice and company details.',
  path: '/imprint',
});

export default function ImprintPage() {
  requireEnabled(legalPages.imprint);
  return (
    <LegalPage title="Imprint">
      <p className="font-medium">{company.legalName}</p>
      <p className="whitespace-pre-line">
        {company.address}
        {'\n'}
        {company.country}
      </p>
      {company.registrationId ? <p>Registration: {company.registrationId}</p> : null}
      {company.vatId ? <p>VAT ID: {company.vatId}</p> : null}
      <p>
        Contact:{' '}
        <a className="underline" href={`mailto:${company.contactEmail}`}>
          {company.contactEmail}
        </a>
      </p>
    </LegalPage>
  );
}
