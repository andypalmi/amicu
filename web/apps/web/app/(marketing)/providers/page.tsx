import type { Metadata } from 'next';
import Link from 'next/link';
import { Globe, MapPin, Server } from 'lucide-react';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@amicu/ui';
import { Container, PageHeader } from '../_components/marketing';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'EU-first provider coverage: Hetzner, OVHcloud, Scaleway, Nebius, RunPod, AWS, Azure, GCP, and self-hosted / SSH. Residency and region are first-class planning inputs.',
  path: '/providers',
});

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Container className="py-20 sm:py-24">
        <PageHeader
          eyebrow="Providers"
          title="EU-first coverage, your residency."
          description="Amicu plans against European-native providers first, reaches the hyperscalers through their EU regions, and supports self-hosted / SSH for full residency control. Residency and region are planning inputs — an optional adapter-owned zone is the third level when a provider needs it."
        />

        <div className="mt-14 grid gap-6 sm:grid-cols-2 lg:grid-cols-3">
          {providers.map((provider) => {
            const isSelfHosted = provider.name.startsWith('Self-hosted');
            const Icon = isSelfHosted ? Server : provider.euNative ? MapPin : Globe;
            return (
              <Card key={provider.name} className="flex flex-col">
                <CardHeader className="gap-2">
                  <div className="flex items-start justify-between gap-3">
                    <div className="flex items-center gap-2">
                      <Icon className="size-5 text-primary" aria-hidden />
                      <CardTitle className="text-lg">{provider.name}</CardTitle>
                    </div>
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
            optional, adapter-owned third level — resolved region-to-zone for
            zone-addressed provider APIs, with an advanced override when you need it.
            Multi-AZ redundancy is an Enterprise HA policy, not a base primitive.
          </p>
          <div className="mt-6">
            <Button asChild variant="outline">
              <Link href="/docs">Read the regions reference</Link>
            </Button>
          </div>
        </section>
      </Container>
    </main>
  );
}
