import type { ReactNode } from 'react';
import { cn } from '@amicu/ui';

/** Constrained page shell with consistent horizontal padding. */
export function Container({
  children,
  className,
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <div className={cn('mx-auto w-full max-w-6xl px-6', className)}>{children}</div>
  );
}

/** Centered page header used at the top of each marketing route. */
export function PageHeader({
  eyebrow,
  title,
  description,
}: {
  eyebrow?: string;
  title: string;
  description: string;
}) {
  return (
    <header className="mx-auto max-w-3xl text-center">
      {eyebrow ? (
        <p className="mb-3 text-sm font-medium uppercase tracking-widest text-muted-foreground">
          {eyebrow}
        </p>
      ) : null}
      <h1 className="text-balance text-4xl font-bold tracking-tight sm:text-5xl">
        {title}
      </h1>
      <p className="mx-auto mt-4 max-w-2xl text-pretty text-lg text-muted-foreground">
        {description}
      </p>
    </header>
  );
}
