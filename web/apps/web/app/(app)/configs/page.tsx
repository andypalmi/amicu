import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { ConfigsConsole } from '@/components/configs/console';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';

export const metadata: Metadata = { title: 'My Configs' };

// Authed "My Configs" page. The session is required server-side (redirects to
// /login otherwise); the console reads the user's saved configurations from the
// API and drives history/diff/restore/load-into-wizard through the
// /api/configs/* BFF. Community-minimum, like the wizard - no tier gating here.
export default async function ConfigsPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-4xl space-y-6 px-6 py-12">
      <PageHeader
        title="My Configs"
        description="Configurations you have saved from the wizard. Review version history, diff any two versions, restore an older version, or load one back into the wizard to keep editing."
      />

      <Suspense fallback={<LoadingState label="Loading configurations…" />}>
        <ConfigsConsole />
      </Suspense>
    </main>
  );
}
