import type { Metadata } from 'next';
import { Suspense } from 'react';
import { requireSession } from '@/lib/auth/session';
import { Wizard } from '@/components/wizard/wizard';
import { PageHeader } from '@/components/common/page-header';
import { LoadingState } from '@/components/common/loading-state';

export const metadata: Metadata = { title: 'Wizard' };

// The guided-wizard surface: pick a preset from the gallery, answer a short
// requirements form seeded from the preset's defaults, then emit ready-to-run
// Amicu HCL (with copy / download / validate) via the /api/presets BFF routes.
// The preset catalog is Community-minimum - tier is enforced by the API, and the
// UI adds no gating of its own.
export default async function WizardPage() {
  await requireSession();

  return (
    <main className="mx-auto w-full max-w-6xl space-y-6 px-6 py-10">
      <PageHeader
        title="Guided wizard"
        description="Start from a preset that matches your use case, answer a few requirements, and get back a ready-to-run Amicu configuration. The wizard emits HCL deterministically from your answers - no metrics are synthesized, and nothing is applied. Copy it, download it, or validate it in place before you use it."
      />

      <Suspense fallback={<LoadingState label="Loading wizard…" />}>
        <Wizard />
      </Suspense>
    </main>
  );
}
