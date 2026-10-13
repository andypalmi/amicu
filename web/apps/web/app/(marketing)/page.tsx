import type { Metadata } from 'next';
import Link from 'next/link';
import { Button } from '@amicu/ui';
import { appName, appTagline, siteUrl } from '@/lib/shared';
import { HclCode, Transcript } from './_components/code';
import { CodePanel, Container, CTABand, FactList, SectionHeader, StatusNote } from './_components/marketing';
import { githubUrl, quickstartUrl } from './_lib/content';
import {
  GPU_LAUNCH,
  HERO_HCL,
  HERO_PLAN,
  LAPTOP_LAUNCH,
  OUTCOMES,
  STEPS,
  SUPPORT,
} from './_lib/landing';
import { pageMetadata } from './_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Infrastructure as code for LLM serving',
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

function PrimaryCtas() {
  return (
    <div className="flex flex-wrap items-center gap-3">
      <Button asChild size="lg">
        <a href={quickstartUrl}>Read the quickstart</a>
      </Button>
      <Button asChild size="lg" variant="outline">
        <Link href="/docs">Read the docs</Link>
      </Button>
    </div>
  );
}

export default function LandingPage() {
  return (
    <main className="flex flex-1 flex-col">
      <script
        type="application/ld+json"
        dangerouslySetInnerHTML={{
          __html: JSON.stringify(jsonLd).replace(/</g, '\\u003c'),
        }}
      />

      {/* Hero: the claim, then the real artifact. */}
      <section>
        <Container className="pt-16 pb-20 sm:pt-24 sm:pb-28">
          <h1 className="max-w-4xl text-balance text-4xl font-semibold tracking-tight sm:text-6xl sm:leading-[1.05]">
            Infrastructure as code for self-hosted LLMs.
          </h1>
          <p className="mt-6 max-w-2xl text-pretty text-lg leading-relaxed text-muted-foreground">
            Describe model, hardware and runtime in one HCL file. Amicu checks fit and data
            residency, shows the plan, then applies it.
          </p>
          <div className="mt-8 flex flex-col gap-4">
            <PrimaryCtas />
            <StatusNote className="self-start">
              Open source under AGPL-3.0. Hosted sign-ups are closed for now.
            </StatusNote>
          </div>

          <div className="mt-14 grid items-start gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.15fr)] sm:mt-20">
            <CodePanel caption="amicu.hcl">
              <HclCode code={HERO_HCL} className="px-5 py-4" />
            </CodePanel>
            <CodePanel caption="terminal">
              <Transcript
                lines={HERO_PLAN}
                reveal
                wrap
                label="Output of amicu plan for the example file"
                className="px-5 py-4"
              />
            </CodePanel>
          </div>
        </Container>
      </section>

      {/* How it works */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeader
            id="how-it-works"
            title="How it works"
            lead="Declare, plan, apply, observe."
          />
          <ol className="mt-14">
            {STEPS.map((step, i) => (
              <li
                key={step.id}
                className="grid gap-6 border-t border-border py-10 md:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] md:gap-12"
              >
                <div>
                  <p className="font-mono text-sm text-muted-foreground">{i + 1}</p>
                  <h3 className="mt-2 text-xl font-semibold tracking-tight">{step.title}</h3>
                  <p className="mt-3 max-w-prose text-pretty leading-relaxed text-muted-foreground">
                    {step.body}
                  </p>
                </div>
                <Transcript
                  lines={step.transcript}
                  wrap
                  label={`Example: ${step.title}`}
                  className="self-start rounded-md border border-border bg-muted/40 px-5 py-4"
                />
              </li>
            ))}
          </ol>
        </Container>
      </section>

      {/* What you get */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeader
            title="From a laptop to a GPU without rewriting your setup"
            lead="Same workflow on a MacBook with llama.cpp and an L4 with vLLM."
          />
          <div className="mt-10 grid gap-4 lg:grid-cols-2">
            <LaunchLine where="On your laptop, llama.cpp" command={LAPTOP_LAUNCH} />
            <LaunchLine where="On Modal in eu-west, vLLM" command={GPU_LAUNCH} />
          </div>
          <FactList className="mt-16" items={OUTCOMES.map((o) => ({ term: o.title, detail: o.body }))} />
        </Container>
      </section>

      {/* Where it runs today */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeader
            id="where-it-runs"
            title="Where it runs today"
            lead="What ships now."
          />
          <FactList className="mt-12" items={SUPPORT.map((x) => ({ term: x.label, detail: x.value }))} />
          <p className="mt-6 text-sm text-muted-foreground">
            Per-provider status and regions are on the{' '}
            <Link href="/providers" className="text-foreground underline underline-offset-4">
              providers page
            </Link>
            .
          </p>
        </Container>
      </section>

      {/* Who it's for, and how to run it */}
      <section className="border-t border-border">
        <Container className="grid gap-12 py-20 sm:py-28 md:grid-cols-2 md:gap-16">
          <div>
            <h2 className="text-2xl font-semibold tracking-tight">Who it is for</h2>
            <p className="mt-4 text-pretty leading-relaxed text-muted-foreground">
              Engineers who run open-weight models themselves: platform teams taking a
              llama.cpp prototype onto real GPUs, and teams in regulated settings who have to
              show where prompts are processed.
            </p>
          </div>
          <div>
            <h2 className="text-2xl font-semibold tracking-tight">Self-host or use the console</h2>
            <p className="mt-4 text-pretty leading-relaxed text-muted-foreground">
              The CLI, planner and server are open source. Build them from source and run
              everything yourself, with state in your project or in your own Postgres. The
              hosted console adds the guided wizard, saved configs and shared workspaces;
              sign-ups are closed during early access, but existing accounts can{' '}
              <Link href="/login" className="text-foreground underline underline-offset-4">
                sign in
              </Link>
              .
            </p>
          </div>
        </Container>
      </section>

      {/* Closing CTA */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <CTABand
            title="Try it on your laptop first."
            lede="Build the CLI, run amicu init and read the plan."
            primary={{ label: 'Read the quickstart', href: quickstartUrl, external: true }}
            secondary={{ label: 'View on GitHub', href: githubUrl, external: true }}
          />
        </Container>
      </section>
    </main>
  );
}

function LaunchLine({ where, command }: { where: string; command: string }) {
  return (
    <figure className="min-w-0 rounded-md border border-border bg-card px-5 py-4">
      <figcaption className="text-sm text-muted-foreground">{where}</figcaption>
      <pre
        tabIndex={0}
        aria-label={`Launch command: ${where}`}
        className="mt-2 whitespace-pre-wrap break-all font-mono text-[0.8125rem] leading-6 text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      >
        <code>{command}</code>
      </pre>
    </figure>
  );
}
