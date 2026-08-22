import type { Metadata } from 'next';
import Link from 'next/link';
import { ArrowRight, Compass, FileCode2 } from 'lucide-react';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { appName, appTagline, gitConfig, siteUrl } from '@/lib/shared';
import { Container } from './_components/marketing';
import { landingFeatures, providers } from './_lib/content';
import { pageMetadata } from './_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Design, deploy, and operate LLM serving stacks',
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
  operatingSystem: 'Web, Linux, macOS',
  offers: {
    '@type': 'Offer',
    price: '0',
    priceCurrency: 'EUR',
  },
  license: 'https://www.gnu.org/licenses/agpl-3.0.html',
  sameAs: [`https://github.com/${gitConfig.user}/${gitConfig.repo}`],
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
      {/* Hero */}
      <section className="border-b border-border">
        <Container className="flex flex-col items-center gap-6 py-24 text-center sm:py-32">
          <Badge variant="outline" className="gap-1.5">
            <span className="inline-block size-1.5 rounded-full bg-primary" />
            Open-source · EU-first inference
          </Badge>
          <h1 className="max-w-3xl text-balance text-4xl font-bold tracking-tight sm:text-6xl">
            Design, deploy, and operate LLM serving stacks.
          </h1>
          <p className="max-w-2xl text-pretty text-lg text-muted-foreground">
            Amicu is a thin, open client over the Rust Management API — guided stack
            design, transparent recommendations, and day-2 operations for EU-first
            inference.
          </p>
          <div className="flex flex-wrap items-center justify-center gap-3">
            <Button asChild size="lg">
              <Link href="/app">
                Get started <ArrowRight />
              </Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <Link href="/docs">Read the docs</Link>
            </Button>
          </div>
        </Container>
      </section>

      {/* Dual-mode teaser */}
      <section className="border-b border-border bg-muted/30">
        <Container className="py-20">
          <div className="mx-auto max-w-2xl text-center">
            <h2 className="text-3xl font-semibold tracking-tight">
              Two ways in. Both open to everyone.
            </h2>
            <p className="mt-3 text-muted-foreground">
              The tier split is about depth of recommendation and team collaboration —
              never about access to the wizard. Community gets the guided flow too.
            </p>
          </div>
          <div className="mx-auto mt-10 grid max-w-4xl gap-6 sm:grid-cols-2">
            <Card>
              <CardHeader>
                <Compass className="size-6 text-primary" aria-hidden />
                <CardTitle className="text-xl">Help me choose</CardTitle>
                <CardDescription>
                  A guided wizard turns your outcome — model, latency, budget,
                  residency — into a recommended stack, with the reasoning shown.
                </CardDescription>
              </CardHeader>
              <CardContent>
                <Button asChild variant="link" className="h-auto p-0">
                  <Link href="/app">
                    Start the wizard <ArrowRight />
                  </Link>
                </Button>
              </CardContent>
            </Card>
            <Card>
              <CardHeader>
                <FileCode2 className="size-6 text-primary" aria-hidden />
                <CardTitle className="text-xl">I know my stack</CardTitle>
                <CardDescription>
                  Drop straight into the HCL editor. Author, validate, and plan / apply
                  declarative infrastructure — the stack is code, not click-ops.
                </CardDescription>
              </CardHeader>
              <CardContent>
                <Button asChild variant="link" className="h-auto p-0">
                  <Link href="/app">
                    Open the editor <ArrowRight />
                  </Link>
                </Button>
              </CardContent>
            </Card>
          </div>
        </Container>
      </section>

      {/* Feature grid */}
      <section className="border-b border-border">
        <Container className="py-20">
          <div className="grid gap-8 sm:grid-cols-2 lg:grid-cols-3">
            {landingFeatures.map((feature) => (
              <div key={feature.title}>
                <h3 className="text-lg font-semibold">{feature.title}</h3>
                <p className="mt-2 text-sm leading-relaxed text-muted-foreground">
                  {feature.body}
                </p>
              </div>
            ))}
          </div>
        </Container>
      </section>

      {/* Provider strip */}
      <section className="border-b border-border bg-muted/30">
        <Container className="py-16">
          <p className="text-center text-sm font-medium uppercase tracking-widest text-muted-foreground">
            Deploy across EU-first providers
          </p>
          <div className="mt-6 flex flex-wrap items-center justify-center gap-x-3 gap-y-3">
            {providers.map((provider) => (
              <Badge
                key={provider.name}
                variant={provider.euNative ? 'secondary' : 'outline'}
                className="text-sm"
              >
                {provider.name}
              </Badge>
            ))}
          </div>
          <div className="mt-8 text-center">
            <Button asChild variant="outline">
              <Link href="/providers">
                See provider coverage <ArrowRight />
              </Link>
            </Button>
          </div>
        </Container>
      </section>

      {/* Closing CTA */}
      <section>
        <Container className="flex flex-col items-center gap-6 py-24 text-center">
          <h2 className="max-w-2xl text-balance text-3xl font-semibold tracking-tight sm:text-4xl">
            Bring your stack under transparent, open control.
          </h2>
          <div className="flex flex-wrap items-center justify-center gap-3">
            <Button asChild size="lg">
              <Link href="/app">Get started</Link>
            </Button>
            <Button asChild size="lg" variant="outline">
              <Link href="/pricing">Compare tiers</Link>
            </Button>
          </div>
        </Container>
      </section>
    </main>
  );
}
