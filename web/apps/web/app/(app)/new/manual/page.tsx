import type { Metadata } from 'next';
import { requireSession } from '@/lib/auth/session';
import { PageHeader } from '@/components/common/page-header';
import { HclResult } from '@/components/wizard/hcl-result';

export const metadata: Metadata = { title: 'New configuration' };

// The "I know my stack" entry (andypalmi/amicu-planning#85): skips the guided
// wizard entirely and drops the user straight into <HclResult> seeded with
// empty HCL, so they can write their own configuration, validate it, and save
// it through the same /api/engine/validate and /api/configs BFF routes the
// guided wizard's confirm step and the classic `/wizard` use.
export default async function ManualConfigPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-6xl space-y-6 px-6 py-10">
      <PageHeader
        title="New configuration"
        description="Write your own Amicu HCL from scratch, validate it against the engine, and save it - nothing is applied."
      />

      <HclResult result={{ hcl: '' }} />
    </main>
  );
}
