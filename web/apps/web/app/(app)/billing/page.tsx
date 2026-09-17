import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { BillingConsole } from '@/components/billing/console';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';
import { PageShell } from '@/components/common/page-shell';

export const metadata: Metadata = { title: 'Billing' };

// Authed billing page. The session is required server-side (redirects to /login
// otherwise); the interactive console reads the user's real billing status from
// the API and drives Stripe checkout / portal through the /api/billing/* BFF.
export default async function BillingPage() {
  await requireSession();

  return (
    <PageShell size="sm">
      <PageHeader title="Billing" description="Manage your subscription and plan." />

      <Suspense fallback={<LoadingState label="Loading billing…" />}>
        <BillingConsole />
      </Suspense>
    </PageShell>
  );
}
