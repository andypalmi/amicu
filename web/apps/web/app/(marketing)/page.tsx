import type { CSSProperties } from 'react';
import type { Metadata } from 'next';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { appName, appTagline, gitConfig, siteUrl } from '@/lib/shared';
import { ApplyDemo } from './_components/apply-demo';
import { InstallTabs } from './_components/install';
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
import { Faq } from '@/components/seo/faq';
import { JsonLd } from '@/components/seo/json-ld';
import { formatDate, pageModified, softwareVersion } from '@/lib/site-facts';
import {
  githubUrl,
  providers,
  quickstartUrl,
  releasesUrl,
  supportedCloudNames,
  type ProviderStatus,
} from './_lib/content';
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
  title: 'Deploy vLLM and llama.cpp from one HCL file',
  description: `amicu is an open-source CLI that deploys vLLM and llama.cpp servers to your own machine or to GPU cloud providers (${supportedCloudNames}) from an HCL file, with plan, apply and destroy.`,
  path: '/',
});

const author = { '@type': 'Person', name: gitConfig.user, url: `https://github.com/${gitConfig.user}` };
const publisher = { '@type': 'Organization', name: appName, url: siteUrl };
const license = 'https://www.gnu.org/licenses/agpl-3.0.html';

const jsonLd = [
  {
    '@context': 'https://schema.org',
    '@type': 'SoftwareApplication',
    name: appName,
    description: appTagline,
    url: siteUrl,
    applicationCategory: 'DeveloperApplication',
    operatingSystem: 'Linux, macOS, Windows',
    softwareVersion,
    downloadUrl: releasesUrl,
    codeRepository: githubUrl,
    programmingLanguage: 'Rust',
    featureList: [
      'Declare models, hardware and deployments in HCL',
      'Plan, apply and destroy with recorded state',
      'Runs vLLM and llama.cpp',
      `Deploys to your own machine or GPU cloud providers (${supportedCloudNames})`,
      'Checks that a model fits the hardware before anything starts',
      'Enforces data residency at plan time',
    ],
    offers: {
      '@type': 'Offer',
      price: '0',
      priceCurrency: 'USD',
    },
    license,
    author,
    publisher,
    sameAs: [githubUrl],
  },
  {
    '@context': 'https://schema.org',
    '@type': 'SoftwareSourceCode',
    name: appName,
    description: appTagline,
    codeRepository: githubUrl,
    programmingLanguage: 'Rust',
    license,
    author,
    url: githubUrl,
  },
];

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
const providersAsOf = pageModified('/providers');
const statusCounts = statusOrder
  .map((s) => `${providers.filter((p) => p.status === s).length} ${statusLabel[s].toLowerCase()}`)
  .join(', ');

export default function LandingPage() {
  return (
    <main className="flex flex-1 flex-col">
      <JsonLd data={jsonLd} />

      {/* Hero: tighter on desktop so headline, subline and the whole terminal sit above the fold. */}
      <HomeSection className="lg:py-10">
        <Headline as="h1">
          Deploy and manage LLMs as code.
        </Headline>
        <p className={cn('mt-6 font-mono text-sm text-muted-foreground sm:text-base lg:mt-4', MEASURE)}>
          On your laptop, your servers or a cloud GPU.
        </p>
        <p className={cn('mt-4 text-base text-muted-foreground sm:text-lg', MEASURE)}>
          amicu is an open-source (AGPL-3.0) command-line tool that deploys vLLM and llama.cpp
          servers to your own machine or to GPU cloud providers ({supportedCloudNames}) from an
          HCL file, with plan, apply and destroy.
        </p>
        <div className="mt-8 flex flex-wrap items-center gap-3 lg:mt-6">
          <Button asChild size="lg">
            <Link href={quickstartUrl}>Get started</Link>
          </Button>
          <Button asChild size="lg" variant="outline">
            <a href="#install">Install</a>
          </Button>
        </div>
        <ApplyDemo className={cn(GAP_HEAD, 'lg:mt-8')} />
      </HomeSection>

      {/* Install */}
      <HomeSection id="install">
        <Headline>How do I install amicu?</Headline>
        <InstallTabs className={cn(GAP_HEAD, 'max-w-3xl')} />
        <p className="mt-6 text-sm text-muted-foreground">
          Other options and manual downloads:{' '}
          <Link href="/install" className="text-foreground underline underline-offset-4">
            install guide
          </Link>
        </p>
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
          rest="your machine or a cloud GPU (Modal, RunPod)."
        />
        <FeatureLinks links={PROVIDER_SWITCH_LINKS} />
      </HomeSection>

      {/* Residency */}
      <HomeSection>
        <Headline>How does amicu enforce data residency?</Headline>
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
        <Headline>Which providers does amicu support?</Headline>
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
          lead={`Apply runs on your hardware and on ${supportedCloudNames} today.`}
          rest={`${previewCount} more, such as Hetzner and Scaleway, are in preview.`}
        />
        {providersAsOf ? (
          <p className="mt-6 text-sm text-muted-foreground">
            As of <time dateTime={providersAsOf}>{formatDate(providersAsOf)}</time>: {statusCounts}.
            See the{' '}
            <Link
              href="/providers#status-table"
              className="text-foreground underline underline-offset-4"
            >
              status table
            </Link>
            .
          </p>
        ) : null}
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

      {/* FAQ */}
      <HomeSection id="faq">
        <Headline>Frequently asked questions</Headline>
        <Faq className={GAP_HEAD} />
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
