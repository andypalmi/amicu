import type { ReactNode } from 'react';
import { cn } from '@amicu/ui';

// Presentational empty-state block for the authenticated consoles. Deliberately
// unwrapped (no `Card`) so callers can drop it inside their own surface, matching
// the ad-hoc "no results" blocks the recommend/monitor consoles already render.
// Styling stays on the shared `@amicu/ui` tokens — no new colors.
export interface EmptyStateProps {
  /** Optional decorative icon (e.g. a lucide icon element) shown above the title. */
  icon?: ReactNode;
  /** Short headline describing the empty condition. */
  title: ReactNode;
  /** Optional supporting copy explaining what to do next. */
  description?: ReactNode;
  /** Optional call-to-action (a button/link). */
  action?: ReactNode;
  className?: string;
}

export function EmptyState({ icon, title, description, action, className }: EmptyStateProps) {
  return (
    <div className={cn('flex flex-col items-center gap-2 py-10 text-center', className)}>
      {icon ? (
        <span className="text-muted-foreground [&_svg]:h-6 [&_svg]:w-6" aria-hidden>
          {icon}
        </span>
      ) : null}
      <p className="text-sm font-medium">{title}</p>
      {description ? (
        <p className="max-w-sm text-sm text-muted-foreground">{description}</p>
      ) : null}
      {action ? <div className="mt-2">{action}</div> : null}
    </div>
  );
}
