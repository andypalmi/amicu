import type { Metadata } from 'next';
import { Check, Minus } from 'lucide-react';
import { ComparisonTable, CTABand, PageHeader, Section, SectionHeader } from '../_components/marketing';
import { providerStatusLabel, providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'Amicu applies to your own machine and Modal today. Hetzner, OVHcloud, Scaleway, Nebius, RunPod, AWS, Azure, GCP and SSH hosts have adapters that are not yet tested live.',
  path: '/providers',
});

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section size="lg">
        <PageHeader
          title="Where amicu applies"
          description="Your machine and Modal are supported. Every other adapter is built but not yet tested live."
        />

        <div className="mt-14">
          <ComparisonTable
            columns={['Provider', 'Category', 'Residency', 'Status']}
            rows={providers.map((p) => [
              <span key="n" className="font-medium">{p.name}</span>,
              <span key="k" className="text-muted-foreground">{p.kind}</span>,
              <span key="r" className="text-muted-foreground">{p.residency}</span>,
              <span key="s" className="inline-flex items-center gap-1.5">
                {p.status === 'supported' ? (
                  <Check className="size-4" aria-hidden />
                ) : (
                  <Minus className="size-4 text-muted-foreground" aria-hidden />
                )}
                {providerStatusLabel[p.status]}
              </span>,
            ])}
          />
          <p className="mt-3 text-sm text-muted-foreground">
            Preview: adapter built, not yet tested live, and not wired into apply.
          </p>
        </div>

        <div className="mt-20">
          <SectionHeader
            title="Plans check residency and region"
            lead="Zone is an optional default the adapter picks from the region, and you can override it."
          />
        </div>

        <div className="mt-16">
          <CTABand
            title="Read the regions reference"
            primary={{ label: 'Open the docs', href: '/docs' }}
          />
        </div>
      </Section>
    </main>
  );
}
