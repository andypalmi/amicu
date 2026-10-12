import type { Metadata } from 'next';
import { company, legalPages } from '@/lib/legal/company';
import { pageMetadata } from '../_lib/seo';
import { Body, LegalPage, requireEnabled } from '../_components/legal';

export const metadata: Metadata = pageMetadata({
  title: 'Privacy Policy',
  description: 'How Amicu handles personal data and cookies.',
  path: '/privacy',
});

export default function PrivacyPage() {
  requireEnabled(legalPages.privacy);
  return (
    <LegalPage title="Privacy Policy" effective={company.privacyEffectiveDate}>
      <p>
        Controller: {company.legalName}, {company.address}, {company.country}. Contact:{' '}
        <a className="underline" href={`mailto:${company.contactEmail}`}>
          {company.contactEmail}
        </a>
        .
      </p>
      {/* Owner/counsel-supplied policy text (lib/legal/company.ts privacyBody). */}
      <Body text={company.privacyBody ?? ''} />

      {/* Factual inventory of what the app sets in the browser. */}
      <h2>Cookies and local storage</h2>
      <p>
        Amicu sets only strictly necessary storage. No analytics or advertising cookies are used.
      </p>
      <ul>
        <li>
          <code>__Host-amicu_at</code> (<code>amicu_at</code> on plain-HTTP local development):
          short-lived access token cookie; strictly necessary for authentication.
        </li>
        <li>
          <code>__Host-amicu_rt</code> (<code>amicu_rt</code> locally): refresh token cookie;
          strictly necessary to keep you signed in.
        </li>
        <li>
          <code>theme</code> (browser localStorage, not a cookie): your light/dark/system
          preference, stored only on your device and never sent to a server.
        </li>
        <li>
          Cloudflare Turnstile (only where enabled, on sign-up, password reset and
          verification-resend forms) loads from challenges.cloudflare.com to tell humans from bots.
        </li>
      </ul>
    </LegalPage>
  );
}
