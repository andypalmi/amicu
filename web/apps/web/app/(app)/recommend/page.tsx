import type { Metadata } from 'next';
import { requireSession } from '@/lib/auth/session';
import { RecommendConsole } from '@/components/recommend/console';
import { PageHeader } from '@/components/common/page-header';

export const metadata: Metadata = { title: 'Recommendations' };

// The advisory recommendations surface: paste a measured metrics + estimate
// snapshot and get back tuning recommendations, each with the evidence,
// confidence, suggested changes, and rejected alternatives behind it (via the
// /api/recommend BFF route). Tier is enforced by the API — a Community caller
// still gets a 200 with an empty report, and the UI degrades gracefully.
export default async function RecommendPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-6xl space-y-6 px-6 py-10">
      <PageHeader
        title="Recommendations & transparency"
        description="Diagnose a serving stack from a measured metrics snapshot and its workload estimate. Every recommendation is advisory and explainable: it shows the evidence that triggered it, the confidence behind it, the changes it suggests, and the alternatives it ruled out. Amicu never auto-applies a recommendation."
      />

      <RecommendConsole />
    </main>
  );
}
