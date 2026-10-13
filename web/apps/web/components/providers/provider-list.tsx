import Link from 'next/link';
import { cn } from '@amicu/ui';
import {
  PROVIDER_STATUS_LABEL,
  PROVIDER_STATUS_NOTE,
  PROVIDER_STATUS_ORDER,
  type Provider,
} from '@/app/(marketing)/_lib/content';

/**
 * Hairline-divided provider list, grouped by status. Each row links to the
 * provider's docs page. `regions` adds a residency column from lg up.
 */
export function ProviderList({
  items,
  regions = false,
}: {
  items: Provider[];
  regions?: boolean;
}) {
  return (
    <div className="not-prose space-y-12">
      {PROVIDER_STATUS_ORDER.map((status) => {
        const rows = items.filter((p) => p.status === status);
        if (rows.length === 0) return null;
        return (
          <section key={status} aria-labelledby={`providers-${status}`}>
            <h2
              id={`providers-${status}`}
              className="font-mono text-xs uppercase tracking-wider text-foreground"
            >
              {PROVIDER_STATUS_LABEL[status]}
            </h2>
            <p className="mt-2 max-w-2xl text-sm text-muted-foreground">
              {PROVIDER_STATUS_NOTE[status]}
            </p>
            <ul className="mt-4 border-b border-border">
              {rows.map((p) => (
                <li key={p.slug} className="border-t border-border">
                  <Link
                    href={['/docs/providers', p.slug].join('/')}
                    className={cn(
                      'group grid grid-cols-[1fr_auto] items-baseline gap-x-6 gap-y-1 py-4 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
                      regions
                        ? 'lg:grid-cols-[12rem_minmax(0,1fr)_minmax(0,18rem)_auto]'
                        : 'lg:grid-cols-[12rem_minmax(0,1fr)_auto]',
                    )}
                  >
                    <span className="text-base font-medium text-foreground">{p.name}</span>
                    <span
                      aria-hidden
                      className="col-start-2 row-start-1 text-muted-foreground transition-transform group-hover:text-foreground motion-safe:group-hover:translate-x-1 lg:order-last lg:col-start-auto lg:row-start-auto"
                    >
                      →
                    </span>
                    <span className="col-span-2 text-sm text-muted-foreground lg:col-span-1">
                      {p.blurb}
                    </span>
                    {regions ? (
                      <span className="hidden text-sm text-muted-foreground lg:block">
                        {p.residency}
                      </span>
                    ) : null}
                  </Link>
                </li>
              ))}
            </ul>
          </section>
        );
      })}
    </div>
  );
}
