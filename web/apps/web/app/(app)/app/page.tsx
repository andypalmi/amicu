import type { Metadata } from 'next';
import { requireSession } from '@/lib/auth/session';
import { EngineConsole } from '@/components/engine/console';
import { PageHeader } from '@/components/common/page-header';
import { PageShell } from '@/components/common/page-shell';

export const metadata: Metadata = { title: 'Console' };

// The authenticated product surface: an HCL editor that validates configs and
// plans changes against the Management API (via the /api/engine/* BFF routes),
// with an optional prior-config diff. Gating is enforced by the API - the UI just
// degrades gracefully on a 403.
export default async function AppHomePage() {
  await requireSession();

  return (
    <PageShell size="lg">
      <PageHeader
        title="Console"
        description="Write your Amicu configuration, validate it, and plan the changes Amicu would apply. Toggle a prior configuration to see a diff."
      />

      <EngineConsole />
    </PageShell>
  );
}
