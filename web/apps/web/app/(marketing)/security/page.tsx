import type { Metadata } from 'next';
import { FactList, PageHeader, Section, CTABand } from '../_components/marketing';
import { pageMetadata } from '../_lib/seo';

const pillars = [
  {
    title: 'Enforced by the API, not the UI',
    body: 'The frontend mirrors tier and role gating for UX - show, lock, upsell - but never enforces it. The Rust Management API re-checks every mutation server-side. Reading a claim never grants an action.',
  },
  {
    title: 'Asymmetric, verifiable sessions',
    body: 'Auth issues asymmetric JWTs with a published JWKS at /.well-known/jwks.json. Tier and role claims verify offline; sessions refresh through the API. No third-party identity dependency in the open core.',
  },
  {
    title: 'Secrets by reference',
    body: 'Amicu works with credential and secret references, not copies of your keys. Provider credentials stay in your secret store; the stack declares what to use, not the secret itself.',
  },
  {
    title: 'Data residency you choose',
    body: 'Residency and region are first-class planning inputs. Plans are checked against the regions of your own machine, Modal and the catalogs for EU-native clouds and the hyperscalers, so a stack that would leave the allowed regions is rejected.',
  },
  {
    title: 'Open source, auditable',
    body: 'The entire frontend - marketing, app, and docs - is AGPL-3.0 in the open monorepo. There is no closed UI hiding paid features; anyone can read and audit exactly what runs in the browser.',
  },
  {
    title: 'Enterprise governance',
    body: 'Org administration, RBAC, SSO, and audit are available on the Enterprise tier for teams that need centralized access control and a verifiable record of who changed what.',
  },
];

export const metadata: Metadata = pageMetadata({
  title: 'Security',
  description:
    'How Amicu handles trust: API-enforced tier gating, asymmetric verifiable sessions, secrets by reference, EU data residency, and a fully open-source auditable frontend.',
  path: '/security',
});

export default function SecurityPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section size="lg">
        <PageHeader
          title="Trust the boundary, not the browser."
          description="The web app renders decisions. The API makes and enforces them."
        />

        <FactList
          className="mt-14"
          items={pillars.map((x) => ({ term: x.title, detail: x.body }))}
        />

        <div className="mt-16">
          <CTABand
            title="The thin-client boundary"
            lede="Compromising the browser cannot grant capabilities the server did not."
            primary={{ label: 'Read the security docs', href: '/docs' }}
          />
        </div>
      </Section>
    </main>
  );
}
