import type { Metadata } from 'next';
import Link from 'next/link';
import { ProviderList } from '@/components/providers/provider-list';
import { HclCode } from '../_components/code';
import { GAP_HEAD, Headline, HomeSection, MEASURE, MockWindow } from '../_components/home';
import { providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'amicu applies to this machine (local, Apple, CPU or on-prem hardware you run amicu on) and Modal today. Other providers are in preview or planned.',
  path: '/providers',
});

const PROVIDER_HCL = `resource "hardware_profile" "gpu" {
  provider = "modal"
  region   = "eu-west"
}`;

const linkClass =
  'text-foreground underline underline-offset-4 decoration-border hover:decoration-foreground';

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <HomeSection>
        <Headline as="h1">Providers</Headline>
        <p className={`mt-6 text-base text-muted-foreground sm:text-lg ${MEASURE}`}>
          A deployment runs wherever its hardware profile&apos;s <code>provider</code> says.
        </p>
        <div className={`${GAP_HEAD} ${MEASURE}`}>
          <MockWindow title="amicu.hcl">
            <HclCode code={PROVIDER_HCL} className="[&_pre]:px-4" />
          </MockWindow>
        </div>
      </HomeSection>

      <HomeSection>
        <ProviderList items={providers} regions />
        <p className={`mt-12 text-sm text-muted-foreground ${MEASURE}`}>
          Moving a deployment is a change to <code>provider</code>. See{' '}
          <Link href="/docs/providers" className={linkClass}>
            Providers
          </Link>{' '}
          and{' '}
          <Link href="/docs/configuration/hardware-profile" className={linkClass}>
            Hardware profile
          </Link>{' '}
          in the docs.
        </p>
      </HomeSection>
    </main>
  );
}
