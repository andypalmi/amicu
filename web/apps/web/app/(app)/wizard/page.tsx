import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { Wizard } from '@/components/wizard/wizard';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';
import { PageShell } from '@/components/common/page-shell';

export const metadata: Metadata = { title: 'Classic preset wizard' };

// The classic preset wizard: pick a preset from the gallery, answer a short
// requirements form seeded from the preset's defaults, then emit ready-to-run
// Amicu HCL (with copy / download / validate) via the /api/presets BFF routes.
// The preset catalog is Community-minimum - tier is enforced by the API, and the
// UI adds no gating of its own.
//
// Superseded as the primary entry point by `/new` (andypalmi/amicu-planning#85),
// which is what the nav's "New config" link now points to - this route stays
// reachable directly because `components/configs/detail.tsx`'s "Load into
// classic wizard" link (?configId=<id>) targets it.
export default async function WizardPage() {
  await requireSession();

  return (
    <PageShell size="lg">
      <PageHeader
        title="Classic preset wizard"
        description="Start from a preset that matches your use case, or from scratch, then answer a few requirements to get back a ready-to-run Amicu configuration. The wizard emits HCL deterministically from your answers - no metrics are synthesized, and nothing is applied. Copy it, download it, or validate it in place before you use it. Looking for the guided flow instead? It lives at /new."
      />

      <Suspense fallback={<LoadingState label="Loading wizard…" />}>
        <Wizard />
      </Suspense>
    </PageShell>
  );
}
