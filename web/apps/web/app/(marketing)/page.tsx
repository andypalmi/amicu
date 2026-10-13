import type { Metadata } from 'next';
import Link from 'next/link';
import { Button } from '@amicu/ui';
import { appName, appTagline, siteUrl } from '@/lib/shared';
import { HclCode, Transcript } from './_components/code';
import { Container } from './_components/marketing';
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

function SectionHeading({ id, title, lead }: { id?: string; title: string; lead?: string }) {
  return (
    <div className="max-w-2xl">
      <h2 id={id} className="scroll-mt-24 text-balance text-3xl font-semibold tracking-tight sm:text-4xl">
        {title}
      </h2>
      {lead ? <p className="mt-4 text-pretty text-lg text-muted-foreground">{lead}</p> : null}
    </div>
  );
}

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
            Infrastructure as code for teams that serve their own LLMs.
          </h1>
          <p className="mt-6 max-w-2xl text-pretty text-lg leading-relaxed text-muted-foreground">
            Describe the model, the hardware and the runtime in one HCL file. Amicu checks
            that it fits and that your data stays where it is allowed to, shows you the exact
            plan, then starts llama.cpp or vLLM on your own machine or on Modal GPUs.
          </p>
          <div className="mt-8 flex flex-col gap-4">
            <PrimaryCtas />
            <p className="text-sm text-muted-foreground">
              Open source under AGPL-3.0. Hosted sign-ups are closed for now.
            </p>
          </div>

          <div className="mt-14 grid gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.15fr)] sm:mt-20">
            <figure className="min-w-0 overflow-hidden rounded-md border border-border bg-card">
              <figcaption className="border-b border-border px-5 py-2.5 font-mono text-xs text-muted-foreground">
                amicu.hcl
              </figcaption>
              <HclCode code={HERO_HCL} className="px-5 py-4" />
            </figure>
            <figure className="flex min-w-0 flex-col overflow-hidden rounded-md border border-border bg-card">
              <figcaption className="border-b border-border px-5 py-2.5 font-mono text-xs text-muted-foreground">
                terminal
              </figcaption>
              <Transcript
                lines={HERO_PLAN}
                reveal
                wrap
                label="Output of amicu plan for the example file"
                className="px-5 py-4"
              />
              <p className="mt-auto border-t border-border px-5 py-4 text-sm leading-relaxed text-muted-foreground">
                The data class keeps customer data in the EU. Change the region to{' '}
                <code className="font-mono text-foreground">us-east</code> and the plan stops
                with a residency error before anything is started.
              </p>
            </figure>
          </div>
        </Container>
      </section>

      {/* How it works */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeading
            id="how-it-works"
            title="How it works"
            lead="The same loop you already use for the rest of your infrastructure, applied to model serving."
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
          <SectionHeading
            title="From a laptop to a GPU without rewriting your setup"
            lead="Prototype with llama.cpp on a MacBook, then point the same workflow at an L4 for vLLM. The plan tells you what will change before it does."
          />
          <div className="mt-10 grid gap-4 lg:grid-cols-2">
            <LaunchLine where="On your laptop, llama.cpp" command={LAPTOP_LAUNCH} />
            <LaunchLine where="On Modal in eu-west, vLLM" command={GPU_LAUNCH} />
          </div>
          <dl className="mt-16">
            {OUTCOMES.map((o) => (
              <div
                key={o.title}
                className="grid gap-2 border-t border-border py-8 md:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] md:gap-12"
              >
                <dt className="text-lg font-semibold tracking-tight">{o.title}</dt>
                <dd className="max-w-prose text-pretty leading-relaxed text-muted-foreground">
                  {o.body}
                </dd>
              </div>
            ))}
          </dl>
        </Container>
      </section>

      {/* Where it runs today */}
      <section className="border-t border-border">
        <Container className="py-20 sm:py-28">
          <SectionHeading
            id="where-it-runs"
            title="Where it runs today"
            lead="What ships now, without the roadmap mixed in."
          />
          <dl className="mt-12">
            {SUPPORT.map((s) => (
              <div
                key={s.label}
                className="grid gap-2 border-t border-border py-6 md:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] md:gap-12"
              >
                <dt className="font-medium">{s.label}</dt>
                <dd className="max-w-prose text-pretty leading-relaxed text-muted-foreground">
                  {s.value}
                </dd>
              </div>
            ))}
          </dl>
          <p className="mt-6 text-sm text-muted-foreground">
            Per-provider regions and residency are listed on the{' '}
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
        <Container className="flex flex-col items-start gap-6 py-20 sm:py-28">
          <h2 className="max-w-2xl text-balance text-3xl font-semibold tracking-tight sm:text-4xl">
            Try it on your laptop first.
          </h2>
          <p className="max-w-2xl text-pretty text-lg text-muted-foreground">
            Build the CLI, run <code className="font-mono text-foreground">amicu init</code>,
            and read the plan for a llama.cpp deployment on your own machine.
          </p>
          <div className="flex flex-wrap items-center gap-3">
            <Button asChild size="lg">
              <a href={quickstartUrl}>Read the quickstart</a>
            </Button>
            <Button asChild size="lg" variant="outline">
              <a href={githubUrl}>View on GitHub</a>
            </Button>
          </div>
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
