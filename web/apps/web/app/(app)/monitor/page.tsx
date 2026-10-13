import type { Metadata } from 'next';
import { requireSession } from '@/lib/auth/session';
import { MonitorDashboard } from '@/components/monitor/dashboard';
import { PageHeader } from '@/components/common/page-header';
import { PageShell } from '@/components/common/page-shell';

export const metadata: Metadata = { title: 'Monitoring' };

// The monitoring surface (measured-vs-estimated): paste a measured metrics
// snapshot and its workload estimate and see the full metric surface - latency,
// throughput, queue, KV, GPU, cost, reliability, all as distributions - laid out
// beside the estimate's ranges and stated assumptions, plus the engine's advisory
// suggested changes (via the /api/monitor BFF route). Tier is enforced by the API:
// a Community caller still gets a 200 with the metric surface and no suggestions.
// The dashboard renders only - it computes no divergence verdict.
export default async function MonitorPage() {
  await requireSession();

  return (
    <PageShell size="lg">
      <PageHeader
        title="Monitoring"
        description="The measured serving surface shown against its workload estimate. Latencies and queue waits are reported as distributions (P50/P90/P99), never a single point presented as truth. The estimate is a range with a stated confidence and assumptions, so any measured-vs-estimated gap stays attributable. Suggested changes are the engine’s advisory output - amicu never auto-applies them."
      />

      <MonitorDashboard />
    </PageShell>
  );
}
