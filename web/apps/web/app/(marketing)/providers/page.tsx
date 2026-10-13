import type { Metadata } from 'next';
import { ProviderRegistry } from '@/components/providers/provider-registry';
import { CTABand, PageHeader, Section } from '../_components/marketing';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'amicu applies to your own machine, your own servers (on_prem) and Modal today. Other providers are in preview or planned.',
  path: '/providers',
});

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section>
        <PageHeader
          title="Providers"
          description="Today the local machine and Modal are supported. Others are in preview or planned."
        />

        <div className="mx-auto mt-14 max-w-5xl">
          <ProviderRegistry items={providers} />
          <p className="mt-6 text-sm text-muted-foreground">
            Supported: wired into <code>amicu apply</code>. Preview: an adapter exists, tested
            against mocks, not wired into apply. Planned: region and pricing data only.
          </p>
        </div>

        <div className="mt-16">
          <CTABand
            title="Regions, residency and zones"
            primary={{ label: 'Provider docs', href: '/docs/providers' }}
          />
        </div>
      </Section>
    </main>
  );
}
