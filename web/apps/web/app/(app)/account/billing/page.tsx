import { redirect } from 'next/navigation';

// The Management API's default Stripe success / cancel / portal-return URLs
// point at /account/billing (STRIPE_*_URL in crates/amicu-api/src/billing.rs),
// but the billing console lives at /billing. Forward, preserving the
// ?checkout=success|cancel outcome so the confirmation banner still shows.
export default async function AccountBillingRedirect({
  searchParams,
}: PageProps<'/account/billing'>) {
  const { checkout } = await searchParams;
  const value = Array.isArray(checkout) ? checkout[0] : checkout;
  redirect(value ? `/billing?checkout=${encodeURIComponent(value)}` : '/billing');
}
