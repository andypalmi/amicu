import type { ReactNode } from 'react';
import { cn } from '@amicu/ui';

// Shared header for the authenticated `(app)` pages. Every console page renders
// the same title/description block (an `h1.text-2xl.font-bold.tracking-tight`
// with a muted description below); this centralises that markup so the pages
// stay consistent and a later design pass can restyle them in one place. The
// optional `actions` slot right-aligns page-level controls (a CTA, a tier badge)
// beside the title block.
export interface PageHeaderProps {
  /** Page title. Rendered as the page's `h1`. */
  title: ReactNode;
  /** Optional muted supporting copy shown under the title. */
  description?: ReactNode;
  /** Optional right-aligned slot for page-level controls (buttons, a badge). */
  actions?: ReactNode;
  className?: string;
}

export function PageHeader({ title, description, actions, className }: PageHeaderProps) {
  const titleBlock = (
    <div>
      <h1 className="text-2xl font-bold tracking-tight">{title}</h1>
      {description ? (
        <p className="text-muted-foreground">{description}</p>
      ) : null}
    </div>
  );

  if (actions) {
    return (
      <div className={cn('flex items-start justify-between gap-4', className)}>
        {titleBlock}
        <div className="flex shrink-0 items-center gap-2">{actions}</div>
      </div>
    );
  }

  return <div className={className}>{titleBlock}</div>;
}
