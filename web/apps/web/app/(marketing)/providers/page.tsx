import type { Metadata } from 'next';
import Link from 'next/link';
import { Table, TableBody, TableCaption, TableCell, TableHead, TableHeader, TableRow } from '@amicu/ui';
import { RunPodSignupLink } from '@/components/runpod-signup';
import { ProviderList } from '@/components/providers/provider-list';
import { HclCode } from '../_components/code';
import { GAP_HEAD, Headline, HomeSection, MEASURE, MockWindow } from '../_components/home';
import { formatDate, pageModified } from '@/lib/site-facts';
import { PROVIDER_STATUS_LABEL, PROVIDER_STATUS_ORDER, providers } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'amicu applies to your hardware (a laptop, a server or on-prem machine you run amicu on), Modal and RunPod today. Other providers are in preview or planned.',
  path: '/providers',
});

const PROVIDER_HCL = `resource "hardware_profile" "gpu" {
  provider = "modal"
  region   = "eu-west"
}`;

const asOf = pageModified('/providers');
const tableRows = [...providers].sort(
  (a, b) => PROVIDER_STATUS_ORDER.indexOf(a.status) - PROVIDER_STATUS_ORDER.indexOf(b.status),
);

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

      <HomeSection id="status-table">
        <Table className="caption-top">
          <TableCaption className="mb-4 mt-0 text-left">
            Provider status{asOf ? <> as of <time dateTime={asOf}>{formatDate(asOf)}</time></> : null}.
            Costs are shown only where the docs state one, and are indicative, not live quotes.
          </TableCaption>
          <TableHeader>
            <TableRow>
              <TableHead>Provider</TableHead>
              <TableHead>Status</TableHead>
              <TableHead>Example GPUs</TableHead>
              <TableHead>Example hourly cost</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {tableRows.map((p) => (
              <TableRow key={p.slug}>
                <TableCell className="font-medium">
                  <Link href={['/docs/providers', p.slug].join('/')} className={linkClass}>
                    {p.name}
                  </Link>
                </TableCell>
                <TableCell>{PROVIDER_STATUS_LABEL[p.status]}</TableCell>
                <TableCell>{p.exampleGpus}</TableCell>
                <TableCell>{p.hourlyCost ?? 'Not listed'}</TableCell>
              </TableRow>
            ))}
          </TableBody>
        </Table>
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
        <RunPodSignupLink className="mt-6" />
      </HomeSection>
    </main>
  );
}
