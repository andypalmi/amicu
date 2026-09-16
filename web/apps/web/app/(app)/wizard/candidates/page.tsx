import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { CandidatePicker } from '@/components/wizard/candidate-picker';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';

export const metadata: Metadata = { title: 'Candidate wizard' };

// Exercise route for the guided-wizard multi-candidate flow
// (andypalmi/amicu-planning#31 questions/candidates, #35 customize + review):
// pick a preset, answer its core questions, rank a handful of candidate
// shapes, then review/edit one's HCL and confirm its plan via
// <CandidateReview>. Deliberately plain, alongside (not replacing) the
// preset-emit wizard at /wizard - #85 owns the polished production guided
// wizard at `/new`/`/new/guided`.
export default async function CandidateWizardPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-6xl space-y-6 px-6 py-10">
      <PageHeader
        title="Candidate wizard"
        description="Pick a preset, answer a few questions, and rank a handful of candidate configurations. Pick one, edit its HCL if you like, review the resulting plan, and confirm it - nothing is applied."
      />

      <Suspense fallback={<LoadingState label="Loading…" />}>
        <CandidatePicker />
      </Suspense>
    </main>
  );
}
