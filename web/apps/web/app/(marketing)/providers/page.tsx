import type { Metadata } from 'next';
import Link from 'next/link';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { PageHeader, Section } from '../_components/marketing';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'Provider-agnostic coverage: Hetzner, OVHcloud, Scaleway, Nebius, RunPod, Modal, AWS, Azure, GCP, and self-hosted / SSH - declare your stack once, run it anywhere. Residency and region are first-class planning inputs.',
  path: '/providers',
});

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section size="lg">
        <PageHeader
          eyebrow="Providers"
          title="Provider-agnostic coverage, your rules."
          description="Amicu plans against any provider through one declarative model - EU-native clouds, hyperscaler regions, or your own hardware over SSH. Pin a fully EU-only stack, an air-gapped one, or a cost-optimized multi-cloud one; residency and region are first-class planning inputs."
        />

        <div className="mt-14 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
          {providers.map((provider) => {
            return (
              <Card key={provider.name} className="flex flex-col">
                <CardHeader className="gap-2">
                  <div className="flex items-start justify-between gap-3">
                    <CardTitle className="text-lg">{provider.name}</CardTitle>
                    {provider.euNative ? (
                      <Badge variant="secondary">EU-native</Badge>
                    ) : null}
                  </div>
                  <p className="text-sm text-muted-foreground">{provider.kind}</p>
                </CardHeader>
                <CardContent className="mt-auto">
                  <p className="text-xs uppercase tracking-wide text-muted-foreground">
                    Residency
                  </p>
                  <p className="mt-1 text-sm">{provider.residency}</p>
                </CardContent>
              </Card>
            );
          })}
        </div>

        <section className="mt-16 rounded-xl border border-border bg-muted/30 p-8">
          <h2 className="text-xl font-semibold tracking-tight">
            Residency → Region → Zone
          </h2>
          <p className="mt-2 max-w-3xl text-sm text-muted-foreground">
            Residency and region are the only required planning inputs. Zone is an
            optional, adapter-owned third level - resolved region-to-zone for
            zone-addressed provider APIs, with an advanced override when you need it.
            Multi-AZ redundancy is an Enterprise HA policy, not a base primitive.
          </p>
          <div className="mt-6">
            <Button asChild variant="outline">
              <Link href="/docs">Read the regions reference</Link>
            </Button>
          </div>
        </section>
      </Section>
    </main>
  );
}
