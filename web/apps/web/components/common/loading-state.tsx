import { Loader2 } from 'lucide-react';
import { cn } from '@amicu/ui';

// Token-styled loading indicator for Suspense fallbacks and pending regions,
// replacing bare "Loading…" text. A labeled, spinning lucide `Loader2` on the
// muted foreground token - no new colors, no external deps.
export interface LoadingStateProps {
  /** Accessible, visible label shown beside the spinner. */
  label?: string;
  className?: string;
}

export function LoadingState({ label = 'Loading…', className }: LoadingStateProps) {
  return (
    <div
      className={cn(
        'flex items-center justify-center gap-2 py-10 text-sm text-muted-foreground',
        className,
      )}
      role="status"
    >
      <Loader2 className="h-4 w-4 animate-spin" aria-hidden />
      <span>{label}</span>
    </div>
  );
}
