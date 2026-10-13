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
  cn,
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@amicu/ui';
import { PageHeader, Section, StatusNote } from '../_components/marketing';
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
    'Amicu tiers - Community, Pro, Enterprise, and Hosted - gated on an individual-vs-team axis. Compare the full feature matrix.',
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
          <span className="sr-only">Locked - upgrade required</span>
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
    <Card
      className={cn(
        'flex flex-col',
        tier.featured && 'border-foreground bg-secondary',
      )}
    >
      <CardHeader className="gap-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-xl">{tier.name}</CardTitle>
        </div>
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
          <Button asChild className="w-full" variant={tier.featured ? 'default' : 'outline'}>
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

export default function PricingPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Section size="lg">
        <PageHeader
          title="One rail, four tiers."
          description="Community is free. Pro, Enterprise and Hosted add team and production features."
        />

        <StatusNote className="mx-auto mt-8 max-w-3xl text-center">
          Hosted sign-ups are closed for now. Community is open source and free.
        </StatusNote>

        <div className="mt-10 grid gap-6 lg:grid-cols-4">
          {tiers.map((tier) => (
            <TierCard key={tier.id} tier={tier} />
          ))}
        </div>

        {/* Full feature matrix */}
        <section className="mt-20">
          <h2 className="text-2xl font-semibold tracking-tight">Compare every tier</h2>
          <p className="mt-2 text-sm text-muted-foreground">
            <Lock className="mr-1 inline size-3.5 align-[-2px]" aria-hidden />
            Locked features are rendered with an upsell, not hidden - the API rejects
            the action regardless of what the UI shows.
          </p>

          <div className="mt-6 overflow-hidden rounded-md border border-border">
            <Table className="min-w-[720px]">
              <TableHeader>
                <TableRow className="bg-muted/40 hover:bg-muted/40">
                  <TableHead className="p-4 text-sm font-medium normal-case tracking-normal text-foreground">
                    Screen / feature
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

          <div className="mt-4 flex flex-wrap gap-x-6 gap-y-2 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <Check className="size-3.5 text-accent-text" aria-hidden /> Included
            </span>
            <span className="flex items-center gap-1.5">
              <Lock className="size-3.5" aria-hidden /> Locked - upgrade to unlock
            </span>
            <span className="flex items-center gap-1.5">as applicable - per plan</span>
          </div>
        </section>
      </Section>
    </main>
  );
}
