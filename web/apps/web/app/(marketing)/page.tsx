import type { CSSProperties } from 'react';
import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { appName, appTagline, siteUrl } from '@/lib/shared';
import { ApplyDemo } from './_components/apply-demo';
import { HclCode, Transcript } from './_components/code';
import {
  DiffBody,
  FeatureLinks,
  FeatureTile,
  Headline,
  HomeSection,
  MockWindow,
  ProofLine,
  Snapshot,
  WindowPair,
  GAP_HEAD,
  MEASURE,
} from './_components/home';
import { githubUrl, providers, quickstartUrl, type ProviderStatus } from './_lib/content';
import {
  APPLY_OUTPUT,
  CONFIG_LINKS,
  DEPLOYMENT_HCL,
  POLICY_LINKS,
  PROVIDER_DIFF,
  PROVIDER_PLAN,
  PROVIDER_SWITCH_LINKS,
  RESIDENCY_HCL,
  RESIDENCY_PLAN,
  TILES,
} from './_lib/landing';
import { pageMetadata } from './_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Infrastructure as code for open-weight LLMs',
  description: appTagline,
  path: '/',
});

// SoftwareApplication structured data for the marketing home. Rendered as an
// inline application/ld+json script (the pattern Next.js recommends); it is
// inert data, not executable JS, and the app CSP allows 'unsafe-inline' in
// script-src regardless, so it is not blocked. `<` is escaped to avoid any
// possibility of breaking out of the script context.
const jsonLd = {
  '@context': 'https://schema.org',
  '@type': 'SoftwareApplication',
  name: appName,
  description: appTagline,
  url: siteUrl,
  applicationCategory: 'DeveloperApplication',
  operatingSystem: 'Linux, macOS',
  offers: {
    '@type': 'Offer',
    price: '0',
    priceCurrency: 'EUR',
  },
  license: 'https://www.gnu.org/licenses/agpl-3.0.html',
  sameAs: [githubUrl],
};

const statusLabel: Record<ProviderStatus, string> = {
  supported: 'Supported',
  preview: 'Preview',
  planned: 'Planned',
};
const statusOrder: ProviderStatus[] = ['supported', 'preview', 'planned'];
const sortedProviders = [...providers].sort(
  (a, b) => statusOrder.indexOf(a.status) - statusOrder.indexOf(b.status),
);
const previewCount = providers.filter((p) => p.status === 'preview').length;

export default function LandingPage() {
  return (
    <main className="flex flex-1 flex-col">
      <script
        type="application/ld+json"
        dangerouslySetInnerHTML={{
          __html: JSON.stringify(jsonLd).replace(/</g, '\\u003c'),
        }}
      />

      {/* Hero */}
      <HomeSection>
        <Headline as="h1">
          Deploy and manage LLMs as code.
        </Headline>
        <p className={cn('mt-6 font-mono text-sm text-muted-foreground sm:text-base', MEASURE)}>
          On your laptop, your servers or a cloud GPU.
        </p>
        <div className="mt-8 flex flex-wrap items-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Get started</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href={githubUrl}>GitHub</a>
          </Button>
        </div>
        <ApplyDemo className={GAP_HEAD} />
      </HomeSection>

      {/* Describe, apply */}
      <HomeSection id="how-it-works">
        <Headline>Describe a model. Get an endpoint.</Headline>
        <WindowPair
          left={
            <MockWindow title="amicu.hcl">
              <HclCode code={DEPLOYMENT_HCL} className="[&_pre]:p-4" />
            </MockWindow>
          }
          right={
            <MockWindow title="terminal">
              <Transcript
                lines={APPLY_OUTPUT}
                wrap
                label="Output of amicu apply"
                className="px-4 py-4"
              />
            </MockWindow>
          }
        />
        <ProofLine
          lead="One file declares model, hardware and policy."
          rest="amicu plans the rest."
        />
        <FeatureLinks links={CONFIG_LINKS} />
      </HomeSection>

      {/* Target switch */}
      <HomeSection>
        <Headline>Laptop or cloud GPU. Same file.</Headline>
        <WindowPair
          left={
            <MockWindow title="amicu.hcl">
              <DiffBody
                lines={PROVIDER_DIFF}
                label="Switching the provider of a hardware profile between a laptop and a cloud GPU"
              />
            </MockWindow>
          }
          right={
            <MockWindow title="terminal">
              <Transcript
                lines={PROVIDER_PLAN}
                wrap
                label="Output of amicu plan after switching the provider to Modal"
                className="px-4 py-4"
              />
            </MockWindow>
          }
        />
        <ProofLine
          lead="Change the provider line to switch targets:"
          rest="your machine or a cloud GPU (Modal)."
        />
        <FeatureLinks links={PROVIDER_SWITCH_LINKS} />
      </HomeSection>

      {/* Residency */}
      <HomeSection>
        <Headline>Your data stays where you say.</Headline>
        <WindowPair
          left={
            <MockWindow title="amicu.hcl">
              <HclCode code={RESIDENCY_HCL} className="[&_pre]:p-4" />
            </MockWindow>
          }
          right={
            <MockWindow title="terminal">
              <Transcript
                lines={RESIDENCY_PLAN}
                wrap
                label="amicu plan rejecting a region outside the EU"
                className="px-4 py-4"
              />
            </MockWindow>
          }
        />
        <ProofLine
          lead="Residency and governance are checked at plan time,"
          rest="before anything deploys."
        />
        <FeatureLinks links={POLICY_LINKS} />
      </HomeSection>

      {/* Providers */}
      <HomeSection>
        <Headline>Pick a provider. Change it later.</Headline>
        <Snapshot>
          <MockWindow title="providers">
            <ul
              className="px-4 font-mono text-sm lg:grid lg:grid-flow-col lg:grid-cols-2 lg:gap-x-8 lg:[grid-template-rows:repeat(var(--rows),auto)]"
              style={{ '--rows': Math.ceil(sortedProviders.length / 2) } as CSSProperties}
            >
              {sortedProviders.map((p, i) => (
                <li
                  key={p.name}
                  className={cn(
                    'border-b border-border last:border-b-0',
                    (i + 1) % Math.ceil(sortedProviders.length / 2) === 0 && 'lg:border-b-0',
                  )}
                >
                  <Link
                    href={['/docs/providers', p.slug].join('/')}
                    className="-mx-4 flex items-center justify-between gap-3 px-4 py-2.5 transition-colors hover:bg-accent hover:text-accent-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring"
                  >
                    <span className="min-w-0 truncate">{p.name}</span>
                    <span
                      className={cn(
                        'shrink-0 rounded-full border px-2.5 py-0.5 text-xs',
                        p.status === 'supported'
                          ? 'border-transparent bg-foreground text-background'
                          : p.status === 'preview'
                            ? 'border-border text-foreground'
                            : 'border-border text-muted-foreground',
                      )}
                    >
                      {statusLabel[p.status]}
                    </span>
                  </Link>
                </li>
              ))}
            </ul>
          </MockWindow>
        </Snapshot>
        <ProofLine
          lead="Apply runs on this machine, on-prem hardware you run amicu on, and Modal today."
          rest={`${previewCount} more, such as Hetzner and Scaleway, are in preview.`}
        />
        <FeatureLinks
          links={[{ label: 'Provider docs', href: '/docs/providers' }]}
          all={{ label: 'All providers', href: '/providers' }}
        />
      </HomeSection>

      {/* Built in */}
      <HomeSection>
        <Headline>Built for operators.</Headline>
        <div className={cn('grid gap-6 md:grid-cols-3', GAP_HEAD)}>
          {TILES.map((t) => (
            <FeatureTile key={t.title} {...t} />
          ))}
        </div>
      </HomeSection>

      {/* Closing CTA */}
      <HomeSection className="flex flex-col items-center text-center">
        <Headline>Plan your first deployment.</Headline>
        <p className={cn('mt-6 font-mono text-sm text-muted-foreground sm:text-base', MEASURE)}>
          Open source, AGPL-3.0. No account needed.
        </p>
        <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Get started</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <Link href="/providers">Providers</Link>
          </Button>
        </div>
      </HomeSection>
    </main>
  );
}
