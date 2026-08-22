import type { Metadata } from 'next';
import Link from 'next/link';
import { Check, Lock, Minus } from 'lucide-react';
import {
  Badge,
  Button,
  Card,
  CardContent,
  CardFooter,
  CardHeader,
  CardTitle,
  cn,
} from '@amicu/ui';
import { Container, PageHeader } from '../_components/marketing';
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
          <Check className="mx-auto size-4 text-primary" aria-hidden />
          <span className="sr-only">Included</span>
        </>
      );
    case 'locked':
      return (
        <>
          <Lock className="mx-auto size-4 text-muted-foreground/60" aria-hidden />
          <span className="sr-only">Locked — upgrade required</span>
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
        tier.featured && 'border-primary shadow-md ring-1 ring-primary',
      )}
    >
      <CardHeader className="gap-3">
        <div className="flex items-center justify-between">
          <CardTitle className="text-xl">{tier.name}</CardTitle>
          {tier.featured ? <Badge>Most popular</Badge> : null}
        </div>
        <p className="text-sm text-muted-foreground">{tier.tagline}</p>
        <p className="text-2xl font-semibold tracking-tight">{tier.priceLabel}</p>
      </CardHeader>
      <CardContent className="flex-1">
        <p className="text-sm text-muted-foreground">{tier.description}</p>
        <ul className="mt-4 space-y-2">
          {tier.highlights.map((item) => (
            <li key={item} className="flex gap-2 text-sm">
              <Check className="mt-0.5 size-4 shrink-0 text-primary" aria-hidden />
              <span>{item}</span>
            </li>
          ))}
        </ul>
      </CardContent>
      <CardFooter>
        <Button
          asChild
          className="w-full"
          variant={tier.featured ? 'default' : 'outline'}
        >
          <Link href={tier.cta.href}>{tier.cta.label}</Link>
        </Button>
      </CardFooter>
    </Card>
  );
}

export default function PricingPage() {
  return (
    <main className="flex flex-1 flex-col">
      <Container className="py-20 sm:py-24">
        <PageHeader
          eyebrow="Pricing"
          title="One rail, four tiers."
          description="Everything a single operator needs stays free in Community. Pro adds advanced recommendations and team collaboration; Enterprise adds governance and production scale; Hosted runs it all for you. Tiers are enforced by the API, never by withholding the open-source UI."
        />

        <div className="mt-14 grid gap-6 lg:grid-cols-4">
          {tiers.map((tier) => (
            <TierCard key={tier.id} tier={tier} />
          ))}
        </div>

        {/* Full feature matrix */}
        <section className="mt-20">
          <h2 className="text-2xl font-semibold tracking-tight">Compare every tier</h2>
          <p className="mt-2 text-sm text-muted-foreground">
            <Lock className="mr-1 inline size-3.5 align-[-2px]" aria-hidden />
            Locked features are rendered with an upsell, not hidden — the API rejects
            the action regardless of what the UI shows.
          </p>

          <div className="mt-6 overflow-x-auto rounded-xl border border-border">
            <table className="w-full min-w-[720px] border-collapse text-sm">
              <thead>
                <tr className="border-b border-border bg-muted/40 text-left">
                  <th className="p-4 font-medium">Screen / feature</th>
                  {tiers.map((tier) => (
                    <th
                      key={tier.id}
                      className="p-4 text-center font-medium first:text-left"
                    >
                      {tier.name}
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {featureMatrix.map((row) => (
                  <tr
                    key={row.feature}
                    className="border-b border-border last:border-0"
                  >
                    <th
                      scope="row"
                      className="p-4 text-left font-normal text-muted-foreground"
                    >
                      {row.feature}
                    </th>
                    <td className="p-4 text-center">
                      <AvailabilityCell value={row.community} />
                    </td>
                    <td className="p-4 text-center">
                      <AvailabilityCell value={row.pro} />
                    </td>
                    <td className="p-4 text-center">
                      <AvailabilityCell value={row.enterprise} />
                    </td>
                    <td className="p-4 text-center">
                      <AvailabilityCell value={row.hosted} />
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>

          <div className="mt-4 flex flex-wrap gap-x-6 gap-y-2 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <Check className="size-3.5 text-primary" aria-hidden /> Included
            </span>
            <span className="flex items-center gap-1.5">
              <Lock className="size-3.5" aria-hidden /> Locked — upgrade to unlock
            </span>
            <span className="flex items-center gap-1.5">as applicable — per plan</span>
          </div>
        </section>
      </Container>
    </main>
  );
}
