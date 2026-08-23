import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { TierGate } from '@/components/tier-gate';
import { OrgConsole } from '@/components/org/console';
import { PageHeader } from '@/components/common/page-header';

export const metadata: Metadata = { title: 'Organization' };

// Authed organization console. The session is required server-side (redirects to
// /login otherwise). The console is an Enterprise feature: <TierGate> shows an
// upgrade upsell to lower tiers WITHOUT calling the API, while Enterprise sessions
// get the live console (membership, RBAC catalog, audit log) driven through the
// /api/org/* BFF. The API re-checks the Enterprise tier and every RBAC rule on
// each request regardless of what is rendered here.
//
// Note: SSO/SCIM federation is deliberately out of scope for v1 (deferred
// post-v1 - see andypalmi/amicu-planning#92); members are managed by user id.
export default async function OrgPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-3xl space-y-6 px-6 py-12">
      <PageHeader
        title="Organization"
        description="Manage members, roles, and review the audit log for your organization."
      />

      <TierGate required="enterprise" feature="The organization console">
        <Suspense fallback={<p className="text-sm text-muted-foreground">Loading…</p>}>
          <OrgConsole />
        </Suspense>
      </TierGate>
    </main>
  );
}
