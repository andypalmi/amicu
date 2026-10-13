import type { Metadata } from 'next';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@amicu/ui';
import { CTABand, PageHeader, Section } from '../_components/marketing';
import { providers, type ProviderStatus } from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Providers',
  description:
    'amicu applies to your own machine and Modal today. Other providers are in preview or planned.',
  path: '/providers',
});

const STATUS_LABEL: Record<ProviderStatus, string> = {
  supported: 'Supported',
  preview: 'Preview',
  planned: 'Planned',
};

export default function ProvidersPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section>
        <PageHeader
          title="Providers"
          description="Today the local machine and Modal are supported. Others are in preview or planned."
        />

        <div className="mx-auto mt-14 max-w-4xl overflow-hidden rounded-md border border-border">
          <Table>
            <TableHeader>
              <TableRow className="bg-muted/40 hover:bg-muted/40">
                <TableHead className="p-4 text-left text-sm font-medium normal-case tracking-normal text-foreground">
                  Provider
                </TableHead>
                <TableHead className="p-4 text-left text-sm font-medium normal-case tracking-normal text-foreground">
                  Status
                </TableHead>
                <TableHead className="hidden p-4 text-left text-sm font-medium normal-case tracking-normal text-foreground sm:table-cell">
                  Regions
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {providers.map((p) => (
                <TableRow key={p.name} className="hover:bg-transparent">
                  <TableHead
                    scope="row"
                    className="p-4 text-left text-sm font-medium normal-case tracking-normal text-foreground"
                  >
                    {p.name}
                    <span className="mt-1 block font-normal text-muted-foreground sm:hidden">
                      {p.residency}
                    </span>
                  </TableHead>
                  <TableCell className="p-4 text-left text-sm">{STATUS_LABEL[p.status]}</TableCell>
                  <TableCell className="hidden p-4 text-left text-sm text-muted-foreground sm:table-cell">
                    {p.residency}
                  </TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        </div>
        <p className="mx-auto mt-4 max-w-4xl text-center text-sm text-muted-foreground">
          Preview: an adapter exists but is not tested live. Planned: region and pricing data
          only.
        </p>

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
