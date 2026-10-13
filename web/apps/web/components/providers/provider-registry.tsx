'use client';

import { useState } from 'react';
import Link from 'next/link';
import { cn } from '@amicu/ui';
import type { Provider, ProviderStatus } from '@/app/(marketing)/_lib/content';
import { Monogram, STATUS_LABEL, StatusPill } from './status-pill';

type Filter = 'all' | ProviderStatus;

const FILTERS: Filter[] = ['all', 'supported', 'preview', 'planned'];

/**
 * Registry-style provider grid with status filter pills. Shared by the
 * marketing /providers page and the docs providers index.
 */
export function ProviderRegistry({ items }: { items: Provider[] }) {
  const [filter, setFilter] = useState<Filter>('all');
  const shown = filter === 'all' ? items : items.filter((p) => p.status === filter);
  const count = (f: Filter) =>
    f === 'all' ? items.length : items.filter((p) => p.status === f).length;

  return (
    <div className="not-prose">
      <div role="group" aria-label="Filter providers by status" className="flex flex-wrap gap-2">
        {FILTERS.map((f) => (
          <button
            key={f}
            type="button"
            aria-pressed={filter === f}
            onClick={() => setFilter(f)}
            className={cn(
              'rounded-full border px-3 py-1 text-sm transition-colors',
              filter === f
                ? 'border-foreground bg-foreground text-background'
                : 'border-border text-muted-foreground hover:bg-accent hover:text-foreground',
            )}
          >
            {f === 'all' ? 'All' : STATUS_LABEL[f]}
            <span className="ml-1.5 text-xs opacity-70">{count(f)}</span>
          </button>
        ))}
      </div>

      <ul className="mt-6 grid grid-cols-1 gap-3 sm:grid-cols-2 lg:grid-cols-3">
        {shown.map((p) => (
          <li key={p.slug} className="min-w-0">
            <Link
              href={['/docs/providers', p.slug].join('/')}
              className="flex h-full flex-col gap-3 rounded-xl border border-border bg-card p-4 text-card-foreground transition-colors hover:bg-accent"
            >
              <span className="flex items-start gap-3">
                <Monogram name={p.name} />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-sm font-semibold">{p.name}</span>
                  <span className="block truncate text-xs text-muted-foreground">{p.kind}</span>
                </span>
                <StatusPill status={p.status} />
              </span>
              <span className="text-sm leading-snug text-muted-foreground">{p.blurb}</span>
              <span className="flex flex-wrap gap-1.5">
                {p.tags.map((t) => (
                  <span
                    key={t}
                    className="rounded-md border border-border px-1.5 py-0.5 text-xs text-muted-foreground"
                  >
                    {t}
                  </span>
                ))}
              </span>
              <span className="mt-auto border-t border-border pt-3 text-xs text-muted-foreground">
                <span className="font-medium text-foreground">Regions </span>
                {p.residency}
              </span>
            </Link>
          </li>
        ))}
      </ul>
    </div>
  );
}
