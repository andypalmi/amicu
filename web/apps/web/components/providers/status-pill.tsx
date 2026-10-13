import { cn } from '@amicu/ui';
import type { ProviderStatus } from '@/app/(marketing)/_lib/content';

export const STATUS_LABEL: Record<ProviderStatus, string> = {
  supported: 'Supported',
  preview: 'Preview',
  planned: 'Planned',
};

const STATUS_STYLE: Record<ProviderStatus, string> = {
  supported: 'bg-primary/12 text-accent-text',
  preview: 'bg-secondary text-secondary-foreground',
  planned: 'bg-muted text-muted-foreground',
};

/** Small uppercase status pill: tinted fill, no border. */
export function StatusPill({ status, className }: { status: ProviderStatus; className?: string }) {
  return (
    <span
      className={cn(
        'inline-flex shrink-0 items-center rounded-full px-2 py-0.5 text-[11px] font-medium uppercase tracking-wide',
        STATUS_STYLE[status],
        className,
      )}
    >
      {STATUS_LABEL[status]}
    </span>
  );
}

/** Square monogram tile standing in for a logo. */
export function Monogram({ name, className }: { name: string; className?: string }) {
  const letter = name.replace(/^This /, '').charAt(0).toUpperCase();
  return (
    <span
      aria-hidden
      className={cn(
        'flex size-10 shrink-0 items-center justify-center rounded-lg border border-border bg-muted font-mono text-sm font-semibold text-foreground',
        className,
      )}
    >
      {letter}
    </span>
  );
}
