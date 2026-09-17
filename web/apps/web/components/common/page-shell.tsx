import type { ReactNode } from 'react';
import { cn } from '@amicu/ui';

// Shared page-level wrapper for the authenticated `(app)` pages. Every console
// page renders the same `<main>` shell - centered, a fixed max width bucket,
// and one consistent vertical rhythm (`py-10 space-y-6`) - so this centralises
// that markup instead of each `page.tsx` hand-rolling its own `<main
// className="mx-auto w-full max-w-…">`. `size` picks the width bucket; pass
// `className` for the rare page-specific override.
export type PageShellSize = 'sm' | 'md' | 'lg';

const SIZE_CLASSES: Record<PageShellSize, string> = {
  sm: 'max-w-2xl',
  md: 'max-w-4xl',
  lg: 'max-w-6xl',
};

export interface PageShellProps {
  /** Width bucket: `sm` (max-w-2xl), `md` (max-w-4xl), or `lg` (max-w-6xl). */
  size?: PageShellSize;
  className?: string;
  children: ReactNode;
}

export function PageShell({ size = 'lg', className, children }: PageShellProps) {
  return (
    <main className={cn('mx-auto w-full space-y-6 px-6 py-10', SIZE_CLASSES[size], className)}>
      {children}
    </main>
  );
}
