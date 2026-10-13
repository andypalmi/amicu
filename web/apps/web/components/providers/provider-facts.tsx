import type { ReactNode } from 'react';
import { PROVIDER_STATUS_LABEL, providers } from '@/app/(marketing)/_lib/content';

/** Header facts for a provider docs page: status, kind, ids, regions. */
export function ProviderFacts({ slug, ids }: { slug: string; ids: string }) {
  const p = providers.find((x) => x.slug === slug);
  if (!p) throw new Error(`ProviderFacts: unknown provider slug "${slug}"`);
  const rows: [string, ReactNode][] = [
    ['Status', PROVIDER_STATUS_LABEL[p.status]],
    ['Kind', p.kind],
    ['Provider id', <code key="i">{ids}</code>],
    ['Regions and residency', p.residency],
  ];
  return (
    <dl className="not-prose my-6 grid grid-cols-1 gap-px overflow-hidden rounded-xl border border-border bg-border sm:grid-cols-2">
      {rows.map(([k, v]) => (
        <div key={k} className="flex min-w-0 flex-col gap-1 bg-card px-4 py-3">
          <dt className="text-xs uppercase tracking-wide text-muted-foreground">{k}</dt>
          <dd className="text-sm">{v}</dd>
        </div>
      ))}
    </dl>
  );
}
