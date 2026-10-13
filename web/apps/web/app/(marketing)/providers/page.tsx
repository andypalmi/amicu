import type { Metadata } from 'next';
import { CTABand, GroupedList, PageHeader, Section } from '../_components/marketing';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'Amicu applies to your own machine and Modal today. Hetzner, OVHcloud, Scaleway, RunPod and SSH hosts have provisioning adapters not yet tested live. AWS, Azure, GCP and Nebius are region and pricing data only.',
  path: '/providers',
});

const CATEGORIES = ['European cloud', 'GPU cloud', 'Hyperscaler', 'Self-hosted'];

export default function ProvidersPage() {
  const supported = providers.filter((p) => p.status === 'supported');
  const preview = providers.filter((p) => p.status === 'preview');
  const planned = providers.filter((p) => p.status === 'planned');
  return (
    <main className="flex flex-1 flex-col">
      <Section>
        <PageHeader
          title="Where amicu applies"
          description="Apply works on your machine and Modal. Some adapters are built but untested live; others are planned."
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
            note="Provisioning adapter built, not yet tested live."
            groups={CATEGORIES.map((c) => ({
              label: c,
              entries: preview
                .filter((p) => p.kind === c)
                .map((p) => ({ name: p.name, note: p.residency })),
            })).filter((g) => g.entries.length > 0)}
          />
          <GroupedList
            title="Planned"
            note="Region and pricing data for planning; provisioning not built yet."
            groups={CATEGORIES.map((c) => ({
              label: c,
              entries: planned
                .filter((p) => p.kind === c)
                .map((p) => ({ name: p.name, note: p.residency })),
            })).filter((g) => g.entries.length > 0)}
          />
        </div>

        <div className="mt-16">
          <CTABand title="How residency, region and zone work" primary={{ label: 'Open the docs', href: '/docs' }} />
        </div>
      </Section>
    </main>
  );
}
