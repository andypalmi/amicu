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
  StackedWindows,
} from './_components/home';
import { githubUrl, providers, quickstartUrl, type ProviderStatus } from './_lib/content';
import {
  APPLY_OUTPUT,
  CONFIG_LINKS,
  DEPLOYMENT_HCL,
  POLICY_LINKS,
  PROVIDER_DIFF,
  PROVIDER_SWITCH_LINKS,
  RESIDENCY_HCL,
  RESIDENCY_PLAN,
  TILES,
} from './_lib/landing';
import { pageMetadata } from './_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Infrastructure as code for self-hosted LLMs',
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
      <HomeSection className="flex flex-col items-center text-center sm:min-h-0 min-h-[calc(100svh-4rem)] justify-center">
        <Headline as="h1" className="max-w-4xl [font-size:clamp(2.75rem,9vw,5.5rem)]">
          Self-hosted LLMs, as code.
        </Headline>
        <p className="mt-6 font-mono text-sm text-muted-foreground sm:text-base">
          On your laptop or a GPU provider.
        </p>
        <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Get started</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href={githubUrl}>GitHub</a>
          </Button>
        </div>
        <MockWindow title="terminal" className="mt-14 w-full max-w-2xl sm:mt-20">
          <ApplyDemo />
        </MockWindow>
      </HomeSection>

      {/* Describe, apply */}
      <HomeSection id="how-it-works">
        <Headline>Describe a model. Get an endpoint.</Headline>
        <StackedWindows
          back={
            <MockWindow title="amicu.hcl">
              <HclCode code={DEPLOYMENT_HCL} className="[&_pre]:px-4 [&_pre]:pt-4 [&_pre]:pb-10" />
            </MockWindow>
          }
          front={
            <MockWindow title="terminal" className="shadow-sm">
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

      {/* Laptop to GPU */}
      <HomeSection>
        <Headline>Start local. Move to a GPU provider with one line.</Headline>
        <MockWindow title="amicu.hcl" className="mt-12 max-w-2xl sm:mt-16">
          <DiffBody lines={PROVIDER_DIFF} label="Changing the provider of a hardware profile" />
        </MockWindow>
        <ProofLine
          lead="llama.cpp on your laptop, vLLM on a GPU provider."
          rest="Same config."
        />
        <FeatureLinks links={PROVIDER_SWITCH_LINKS} />
      </HomeSection>

      {/* Residency */}
      <HomeSection>
        <Headline>Your data stays where you say.</Headline>
        <StackedWindows
          back={
            <MockWindow title="amicu.hcl">
              <HclCode code={RESIDENCY_HCL} className="[&_pre]:px-4 [&_pre]:pt-4 [&_pre]:pb-10" />
            </MockWindow>
          }
          front={
            <MockWindow title="terminal" className="shadow-sm">
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
        <MockWindow title="providers" className="mt-12 max-w-2xl sm:mt-16">
          <ul className="px-4 font-mono text-sm">
            {sortedProviders.map((p) => (
              <li
                key={p.name}
                className="flex items-center justify-between gap-3 border-b border-border py-2.5 last:border-b-0"
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
              </li>
            ))}
          </ul>
        </MockWindow>
        <ProofLine
          lead="Apply runs on your machine and Modal today."
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
        <div className="mt-12 grid gap-4 sm:mt-16 md:grid-cols-3">
          {TILES.map((t) => (
            <FeatureTile key={t.title} {...t} />
          ))}
        </div>
      </HomeSection>

      {/* Closing CTA */}
      <HomeSection className="flex flex-col items-center text-center">
        <Headline>Plan your first deployment.</Headline>
        <p className="mt-6 font-mono text-sm text-muted-foreground sm:text-base">
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
