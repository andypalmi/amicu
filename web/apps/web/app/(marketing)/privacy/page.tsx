import type { Metadata } from 'next';
import { company, legalPages } from '@/lib/legal/company';
import { pageMetadata } from '../_lib/seo';
import { LegalPage, requireEnabled } from '../_components/legal';

export const metadata: Metadata = pageMetadata({
  title: 'Privacy Policy',
  description: 'How amicu handles personal data and cookies.',
  path: '/privacy',
});

const link = 'underline';

export default function PrivacyPage() {
  requireEnabled(legalPages.privacy);
  const email = company.privacyEmail;
  const controller = company.controllerName ?? 'the amicu maintainer';
  return (
    <LegalPage title="Privacy Policy" effective={company.privacyEffectiveDate}>
      <p>
        This policy explains what personal data amicu (the open-source CLI, the website at
        amicu.dev and the hosted web app at app.amicu.dev) handles, why, and what your rights
        are. We aim to collect as little as possible. The amicu CLI runs on your own machine and
        sends nothing to us.
      </p>

      <h2>Who is responsible</h2>
      <p>
        The data controller is {controller}, an individual based in Italy. For anything about
        your personal data, write to{' '}
        <a className={link} href={`mailto:${email}`}>
          {email}
        </a>
        . For general questions use{' '}
        <a className={link} href={`mailto:${company.contactEmail}`}>
          {company.contactEmail}
        </a>
        .
      </p>

      <h2>What we collect and why</h2>
      <ul>
        <li>
          <strong>Account data</strong> (hosted app only): your email address and a hashed
          password when you sign up, plus the account and configuration data you create. Legal
          basis: performing the service you asked for (GDPR Art. 6(1)(b)).
        </li>
        <li>
          <strong>Operational and security data</strong>: standard server and network logs such
          as IP address, user agent and request time, kept by our hosting provider to run the
          service and defend it against abuse. Legal basis: legitimate interests (Art. 6(1)(f)).
        </li>
        <li>
          <strong>Website analytics</strong>: aggregate, cookieless page-view statistics (see
          below) to understand which pages are useful. Legal basis: legitimate interests.
        </li>
        <li>
          <strong>Error reports</strong>: technical details when the website or web app crashes
          (see below), so we can fix bugs. Legal basis: legitimate interests.
        </li>
        <li>
          <strong>Messages you send us</strong> by email, kept as long as needed to answer you.
        </li>
        <li>
          <strong>Billing data</strong>: paid plans are not on sale yet. When they are, payments
          will be handled by a payment provider and this policy will be updated before then.
        </li>
      </ul>
      <p>
        We do not sell your personal data, we do not use it for advertising, and we do not make
        automated decisions with legal effects about you.
      </p>

      <h2>Service providers (processors)</h2>
      <ul>
        <li>
          <strong>Cloudflare</strong> hosts and delivers the website and web app (Workers/CDN).
          It also provides Cloudflare Web Analytics, a cookieless service that reports page
          views and performance without cookies or cross-site tracking, and Turnstile, a bot
          check used on sign-up, password-reset and verification forms.
        </li>
        <li>
          <strong>PostHog (EU cloud)</strong> provides cookieless product
          analytics. It runs in memory only: nothing is written to cookies or local storage,
          our PostHog project is configured to discard client IP addresses (the IP may be used
          momentarily to derive coarse location before being discarded), there is no session
          recording, no autocapture of what you type, and it stays off if
          your browser sends Do Not Track or Global Privacy Control.
        </li>
        <li>
          <strong>Sentry</strong> receives error reports. We turn off sending personal data to
          it: the SDK is set not to send personal data, our Sentry project is configured not to
          store IP addresses, there are no user identifiers and no session replay, and request
          bodies, cookies, headers and query strings are removed before an event is sent.
        </li>
        <li>
          Our own API and database that store your account and configuration data, run on
          cloud infrastructure we operate.
        </li>
      </ul>

      <h2>Cookies and local storage</h2>
      <p>
        amicu uses only strictly necessary storage. It sets no analytics or advertising cookies,
        so no consent banner is shown.
      </p>
      <ul>
        <li>
          <code>__Host-amicu_at</code> (<code>amicu_at</code> on plain-HTTP local development):
          short-lived access token cookie; strictly necessary for authentication. Set only when
          you sign in.
        </li>
        <li>
          <code>__Host-amicu_rt</code> (<code>amicu_rt</code> locally): refresh token cookie;
          strictly necessary to keep you signed in. Set only when you sign in.
        </li>
        <li>
          <code>theme</code> (browser local storage, not a cookie): your light/dark/system
          preference, stored only on your device and never sent to a server.
        </li>
        <li>
          Cloudflare Turnstile, where enabled, loads from challenges.cloudflare.com on sign-up,
          password-reset and verification forms to tell humans from bots. It may set a
          short-lived Cloudflare cookie inside its own iframe, strictly for bot protection.
        </li>
      </ul>

      <h2>Do Not Track and Global Privacy Control</h2>
      <p>
        We honour the Do Not Track and Global Privacy Control browser signals: when either is
        on, our product analytics does not run. Cloudflare Web Analytics does not use cookies
        or identify individual visitors.
      </p>

      <h2>How long we keep data</h2>
      <ul>
        <li>Account and configuration data: while your account exists, then deleted on request or when you close it, except where the law requires us to keep something.</li>
        <li>Analytics and error reports: kept by the providers for a limited period, in aggregate or without personal identifiers, and no longer than needed for these purposes.</li>
        <li>Server logs: kept briefly by the hosting provider.</li>
        <li>Emails you send us: as long as needed to deal with your request.</li>
      </ul>

      <h2>International transfers</h2>
      <p>
        Cloudflare and Sentry are US companies and process data globally. Where personal data
        leaves the European Economic Area, we rely on the EU-US Data Privacy Framework where the
        provider is certified, and otherwise on the European Commission&apos;s Standard
        Contractual Clauses. PostHog data is hosted in the EU.
      </p>

      <h2>Your rights (GDPR and Italian law)</h2>
      <p>
        If you are in the EU/EEA or UK you can ask us to access, correct, delete or restrict
        your personal data, to receive it in a portable format, and to object to processing
        based on legitimate interests. Write to{' '}
        <a className={link} href={`mailto:${email}`}>
          {email}
        </a>
        ; we answer within one month. You can also complain to the Italian data protection
        authority, the Garante per la protezione dei dati personali (
        <a className={link} href="https://www.garanteprivacy.it">
          garanteprivacy.it
        </a>
        ), or to the authority in your own country.
      </p>

      <h2>California and other US states</h2>
      <p>
        We do not sell or share (as defined by the CCPA/CPRA) personal information, and we do
        not use it for cross-context behavioural advertising. California residents may request
        to know, delete or correct their personal information, and will not be treated
        differently for doing so. Use the address above.
      </p>

      <h2>Children</h2>
      <p>
        amicu is not directed to children under 16 and accounts require you to be 18 or older
        (see the Terms). We do not knowingly collect personal data from children. If you believe
        a child has given us data, contact us and we will delete it.
      </p>

      <h2>Changes</h2>
      <p>
        We will update this page when our practices change (for example when paid plans launch)
        and change the effective date above.
      </p>
    </LegalPage>
  );
}
