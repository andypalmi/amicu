import type { Metadata } from 'next';
import Link from 'next/link';
import { Button } from '@amicu/ui';
import { appName, appTagline, siteUrl } from '@/lib/shared';
import { HclCode, Transcript } from './_components/code';
import { CodePanel, Container, CTABand, Section, FactList, SectionHeader } from './_components/marketing';
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
        <Container className="pt-16 pb-20 text-center sm:pt-24 sm:pb-28">
          <h1 className="mx-auto max-w-4xl text-balance text-4xl font-semibold tracking-tight sm:text-6xl sm:leading-[1.05]">
            Infrastructure as code for self-hosted LLMs.
          </h1>
          <p className="mx-auto mt-6 max-w-2xl text-pretty text-lg leading-relaxed text-muted-foreground">
            Terraform for self-hosted inference. Describe the model, hardware and runtime in
            HCL, read the plan, then apply it to your machine or Modal.
          </p>
          <div className="mt-8 flex flex-wrap items-center justify-center gap-3">
            <Button asChild size="lg">
              <Link href={quickstartUrl}>Quickstart</Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href={githubUrl}>GitHub</a>
            </Button>
          </div>

          <div className="mt-14 grid items-start gap-4 text-left sm:mt-20 lg:grid-cols-2">
            <CodePanel caption="amicu.hcl">
              <HclCode code={HERO_HCL} className="[&_pre]:px-5 [&_pre]:py-4" />
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
      <Section>
        <SectionHeader id="how-it-works" title="How it works" />
        <ol className="mx-auto mt-14 max-w-3xl">
          {STEPS.map((step, i) => (
            <li
              key={step.id}
              className="flex flex-col gap-6 border-t border-border py-10 text-center"
            >
              <div>
                <p className="font-mono text-sm text-muted-foreground">{i + 1}</p>
                <h3 className="mt-2 text-xl font-semibold tracking-tight">{step.title}</h3>
                <p className="mx-auto mt-3 max-w-prose text-pretty leading-relaxed text-muted-foreground">
                  {step.body}
                </p>
              </div>
              <Transcript
                lines={step.transcript}
                wrap
                label={`Example: ${step.title}`}
                className="rounded-md border border-border bg-muted/40 px-5 py-4 text-left"
              />
            </li>
          ))}
        </ol>
      </Section>

      {/* What you get */}
      <Section>
        <SectionHeader
          title="From a laptop to a GPU"
          lead="The same workflow on a MacBook with llama.cpp and an L4 with vLLM."
        />
        <div className="mt-10 grid gap-4 text-left lg:grid-cols-2">
          <LaunchLine where="On your laptop, llama.cpp" command={LAPTOP_LAUNCH} />
          <LaunchLine where="On Modal in eu-west, vLLM" command={GPU_LAUNCH} />
        </div>
        <FactList className="mt-16" items={OUTCOMES.map((o) => ({ term: o.title, detail: o.body }))} />
      </Section>

      {/* Where it runs today */}
      <Section>
        <SectionHeader id="where-it-runs" title="Where it runs today" />
        <FactList className="mt-12" items={SUPPORT.map((x) => ({ term: x.label, detail: x.value }))} />
        <p className="mt-6 text-center text-sm text-muted-foreground">
          Provider status and regions are on the{' '}
          <Link href="/providers" className="text-foreground underline underline-offset-4">
            providers page
          </Link>
          .
        </p>
      </Section>

      {/* Who it is for, and how to run it */}
      <Section>
        <SectionHeader title="Run it yourself" />
        <div className="mx-auto mt-6 max-w-2xl space-y-4 text-pretty text-center leading-relaxed text-muted-foreground">
          <p>
            For engineers who run open-weight models themselves, including teams that have to
            show where prompts are processed.
          </p>
          <p>
            The CLI, planner and server are open source. Build them from source and keep state
            in your project or your own Postgres. No account is needed.
          </p>
        </div>
      </Section>

      {/* Closing CTA */}
      <Section>
        <CTABand
          title="Try it on your laptop first."
          lede="Install the CLI, run amicu init and read the plan."
          primary={{ label: 'Quickstart', href: quickstartUrl }}
          secondary={{ label: 'GitHub', href: githubUrl, external: true }}
        />
      </Section>
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
