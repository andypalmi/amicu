import type { Metadata } from 'next';
import Link from 'next/link';
import { company, legalPages } from '@/lib/legal/company';
import { pageMetadata } from '../_lib/seo';
import { LegalPage, requireEnabled } from '../_components/legal';

export const metadata: Metadata = pageMetadata({
  title: 'Terms of Service',
  description: 'Terms governing use of amicu.',
  path: '/terms',
});

const link = 'underline';

export default function TermsPage() {
  requireEnabled(legalPages.terms);
  const email = company.contactEmail;
  return (
    <LegalPage title="Terms of Service" effective={company.termsEffectiveDate}>
      <p>
        These terms cover your use of the amicu website, hosted web app and API at amicu.dev and
        app.amicu.dev (the &quot;Service&quot;). The amicu CLI and source code are separate: they
        are open source and governed by their own licence (AGPL-3.0, see the repository).
      </p>

      <h2>Who may use the Service</h2>
      <p>
        You must be 18 or older, or of the age of majority where you live if that is higher, to
        create an account or buy anything. If you use amicu for an organisation, you confirm you
        may bind it to these terms.
      </p>

      <h2>Beta status</h2>
      <p>
        The hosted Service is in beta. Features can change or disappear, and it may be
        unavailable or contain errors. Paid plans are not on sale yet; when they are, price,
        billing and cancellation terms will be shown before you buy and will form part of these
        terms.
      </p>

      <h2>Your account</h2>
      <ul>
        <li>Give accurate information and keep your credentials secure; you are responsible for activity under your account.</li>
        <li>Tell us promptly if you suspect unauthorised access.</li>
        <li>You may close your account at any time by contacting us.</li>
      </ul>

      <h2>Acceptable use</h2>
      <p>
        Do not use the Service to break the law, attack or overload it or other systems, probe
        for vulnerabilities without permission, circumvent limits or access controls, or resell
        access without our agreement. We may suspend accounts that do.
      </p>

      <h2>Your content</h2>
      <p>
        You keep ownership of the configurations and other content you submit. You give us a
        limited licence to store and process it to operate the Service for you. Do not submit
        secrets you are not willing to store with us; configurations should reference secrets,
        not contain them.
      </p>

      <h2>Privacy</h2>
      <p>
        How we handle personal data is described in the{' '}
        <Link className={link} href="/privacy">
          Privacy Policy
        </Link>
        .
      </p>

      <h2>No warranty and limits of liability</h2>
      <p>
        The Service is provided &quot;as is&quot; and &quot;as available&quot;. Infrastructure
        plans and recommendations are generated aids; you are responsible for reviewing them
        before applying them to your own systems and cloud accounts. To the maximum extent
        permitted by law, we are not liable for indirect or consequential loss, and our total
        liability for any claim is limited to the amount you paid for the Service in the 12
        months before the claim (zero for free use). Nothing in these terms excludes liability
        that cannot be excluded by law, including for intent, gross negligence, death or
        personal injury.
      </p>

      <h2>Consumers</h2>
      <p>
        If you are a consumer, these terms do not take away any mandatory rights you have under
        the consumer protection law of your country of residence, including, where applicable,
        the Italian Consumer Code and EU consumer rules.
      </p>

      <h2>Changes and termination</h2>
      <p>
        We may update these terms or the Service; we will change the effective date above and, for
        material changes, give reasonable notice. We may suspend or end the Service or your
        access, for example for breach of these terms. You may stop using it at any time.
      </p>

      <h2>Governing law</h2>
      <p>
        These terms are governed by the laws of Italy. Courts in Italy have jurisdiction, without
        affecting any mandatory right of a consumer to bring a claim in the courts of their
        country of residence.
      </p>

      <h2>Contact</h2>
      <p>
        Questions about these terms:{' '}
        <a className={link} href={`mailto:${email}`}>
          {email}
        </a>
        .
      </p>
    </LegalPage>
  );
}
