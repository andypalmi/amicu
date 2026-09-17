import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { GuidedWizard } from '@/components/wizard/guided/guided-wizard';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';

export const metadata: Metadata = { title: 'Guided wizard' };

// The guided-wizard surface (andypalmi/amicu-planning#85): a deterministic,
// five-step stepper (preset -> questions -> candidates -> review -> confirm)
// over the same preset/recommend/configs BFF routes the classic `/wizard`
// already uses. No sessionStorage persistence - a hard refresh past step 1
// restarts the flow, same as `/wizard`. `step`/`preset`/`candidate` are carried
// as URL search params by <GuidedWizard>; everything else lives in its own
// component state.
export default async function GuidedWizardPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-6xl space-y-6 px-6 py-10">
      <PageHeader
        title="Guided wizard"
        description="Pick a preset, answer a few questions, and choose from a handful of ranked candidate stacks. Review and edit the resulting HCL, confirm its plan, then save it - nothing is applied."
      />

      <Suspense fallback={<LoadingState label="Loading wizard…" />}>
        <GuidedWizard />
      </Suspense>
    </main>
  );
}
