import type { Metadata } from 'next';
import { CTABand, GroupedList, PageHeader, Section, SectionHeader } from '../_components/marketing';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'Amicu applies to your own machine and Modal today. Hetzner, OVHcloud, Scaleway, Nebius, RunPod, AWS, Azure, GCP and SSH hosts have adapters that are not yet tested live.',
  path: '/providers',
});

const CATEGORIES = ['European cloud', 'GPU cloud', 'Hyperscaler', 'Self-hosted'];

export default function ProvidersPage() {
  const supported = providers.filter((p) => p.status === 'supported');
  const preview = providers.filter((p) => p.status === 'preview');
  return (
    <main className="flex flex-1 flex-col">
      <Section size="lg">
        <PageHeader
          title="Where amicu applies"
          description="Your machine and Modal are supported. Every other adapter is built but not yet tested live."
        />

        <div className="mt-14 space-y-12">
          <GroupedList
            title="Supported"
            groups={[
              {
                entries: supported.map((p) => ({ name: p.name, detail: p.kind, note: p.residency })),
              },
            ]}
          />
          <GroupedList
            title="Preview"
            note="Adapter built, not yet tested live."
            groups={CATEGORIES.map((c) => ({
              label: c,
              entries: preview
                .filter((p) => p.kind === c)
                .map((p) => ({ name: p.name, note: p.residency })),
            })).filter((g) => g.entries.length > 0)}
          />
        </div>

        <div className="mt-20">
          <SectionHeader
            title="Plans check residency and region"
            lead="Zone is an optional default the adapter picks from the region, and you can override it."
          />
        </div>

        <div className="mt-16">
          <CTABand title="Read the regions reference" primary={{ label: 'Open the docs', href: '/docs' }} />
        </div>
      </Section>
    </main>
  );
}
