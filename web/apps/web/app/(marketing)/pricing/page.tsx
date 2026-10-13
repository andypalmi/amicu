import type { Metadata } from 'next';
import Link from 'next/link';
import { Check, Lock, Minus } from 'lucide-react';
import {
  Button,
  Card,
  CardContent,
  CardFooter,
  CardHeader,
  CardTitle,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@amicu/ui';
import { PageHeader, Section, SectionHeader } from '../_components/marketing';
import {
  type Availability,
  featureMatrix,
  type Tier,
  tiers,
} from '../_lib/content';
import { pageMetadata } from '../_lib/seo';

export const metadata: Metadata = pageMetadata({
  title: 'Pricing',
  description:
    'amicu is free and open source under AGPL-3.0. Team and enterprise plans are planned.',
  path: '/pricing',
});

function AvailabilityCell({ value }: { value: Availability }) {
  switch (value) {
    case 'yes':
      return (
        <>
          <Check className="mx-auto size-4 text-accent-text" aria-hidden />
          <span className="sr-only">Included</span>
        </>
      );
    case 'locked':
      return (
        <>
          <Lock className="mx-auto size-4 text-muted-foreground/60" aria-hidden />
          <span className="sr-only">Not included</span>
        </>
      );
    case 'asApplicable':
      return <span className="text-xs text-muted-foreground">as applicable</span>;
    case 'no':
      return (
        <>
          <Minus className="mx-auto size-4 text-muted-foreground/40" aria-hidden />
          <span className="sr-only">Not available</span>
        </>
      );
  }
}

function TierCard({ tier }: { tier: Tier }) {
  return (
    <Card className="flex flex-col text-left">
      <CardHeader className="gap-3">
        <CardTitle className="text-xl">{tier.name}</CardTitle>
        <p className="text-sm text-muted-foreground">{tier.tagline}</p>
        <p className="text-2xl font-semibold tracking-tight">{tier.priceLabel}</p>
      </CardHeader>
      <CardContent className="flex-1">
        <p className="text-sm text-muted-foreground">{tier.description}</p>
        <ul className="mt-4 space-y-2">
          {tier.highlights.map((item) => (
            <li key={item} className="flex gap-2 text-sm">
              <Check className="mt-0.5 size-4 shrink-0 text-accent-text" aria-hidden />
              <span>{item}</span>
            </li>
          ))}
        </ul>
      </CardContent>
      <CardFooter>
        {tier.cta.href ? (
          <Button asChild className="w-full" variant="outline">
            <Link href={tier.cta.href}>{tier.cta.label}</Link>
          </Button>
        ) : (
          <Button className="w-full" variant="outline" disabled>
            {tier.cta.label}
          </Button>
        )}
      </CardFooter>
    </Card>
  );
}

const faq = [
  {
    q: 'Can I use amicu at work?',
    a: 'Yes. The AGPL-3.0 allows use for any purpose, including inside a company.',
  },
  {
    q: 'Do my configs and models fall under the AGPL?',
    a: 'No. The license covers the amicu source, not the HCL files you write with it.',
  },
  {
    q: 'When does the AGPL require me to share code?',
    a: 'If you modify amicu and let others use that version over a network, you must offer them its source.',
  },
  {
    q: 'Is there a commercial license?',
    a: 'Not yet. If the AGPL does not fit your use, contact us.',
  },
];

export default function PricingPage() {
  const [community, ...planned] = tiers;
  return (
    <main className="flex flex-1 flex-col">
      <Section>
        <PageHeader title="Pricing" description="amicu is free and open source under AGPL-3.0." />

        <div className="mx-auto mt-12 max-w-xl">
          <TierCard tier={community} />
        </div>
      </Section>

      <Section>
        <SectionHeader
          title="Team and enterprise"
          lead="Planned. Contact us if you want to talk about it."
        />
        <div className="mt-10 grid gap-6 text-left lg:grid-cols-3">
          {planned.map((tier) => (
            <TierCard key={tier.id} tier={tier} />
          ))}
        </div>

        <section className="mt-20">
          <h2 className="text-center text-2xl font-semibold tracking-tight">Compare tiers</h2>
          <div className="mt-6 overflow-x-auto rounded-md border border-border">
            <Table className="min-w-[720px]">
              <TableHeader>
                <TableRow className="bg-muted/40 hover:bg-muted/40">
                  <TableHead className="p-4 text-sm font-medium normal-case tracking-normal text-foreground">
                    Feature
                  </TableHead>
                  {tiers.map((tier) => (
                    <TableHead
                      key={tier.id}
                      className="p-4 text-center text-sm font-medium normal-case tracking-normal text-foreground first:text-left"
                    >
                      {tier.name}
                    </TableHead>
                  ))}
                </TableRow>
              </TableHeader>
              <TableBody>
                {featureMatrix.map((row) => (
                  <TableRow key={row.feature} className="hover:bg-transparent">
                    <TableHead
                      scope="row"
                      className="p-4 text-left text-sm font-normal normal-case tracking-normal text-muted-foreground"
                    >
                      {row.feature}
                    </TableHead>
                    <TableCell className="p-4 text-center">
                      <AvailabilityCell value={row.community} />
                    </TableCell>
                    <TableCell className="p-4 text-center">
                      <AvailabilityCell value={row.pro} />
                    </TableCell>
                    <TableCell className="p-4 text-center">
                      <AvailabilityCell value={row.enterprise} />
                    </TableCell>
                    <TableCell className="p-4 text-center">
                      <AvailabilityCell value={row.hosted} />
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          </div>

          <div className="mt-4 flex flex-wrap justify-center gap-x-6 gap-y-2 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <Check className="size-3.5 text-accent-text" aria-hidden /> Included
            </span>
            <span className="flex items-center gap-1.5">
              <Lock className="size-3.5" aria-hidden /> Not in this tier
            </span>
          </div>
        </section>
      </Section>

      <Section>
        <SectionHeader title="AGPL questions" />
        <dl className="mx-auto mt-10 max-w-2xl divide-y divide-border border-y border-border text-center">
          {faq.map((f) => (
            <div key={f.q} className="py-6">
              <dt className="font-medium">{f.q}</dt>
              <dd className="mt-2 text-pretty leading-relaxed text-muted-foreground">{f.a}</dd>
            </div>
          ))}
        </dl>
      </Section>
    </main>
  );
}
