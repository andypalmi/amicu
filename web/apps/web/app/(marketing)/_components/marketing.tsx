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
        <p className="mb-3 text-sm font-medium text-muted-foreground">
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

const SECTION_SIZES = {
  md: 'py-16',
  lg: 'py-24',
} as const;

/**
 * Full-width marketing band with a bottom rule. Two vertical sizes only
 * (`md` = py-16, `lg` = py-24) so rhythm stays consistent; `tone="muted"`
 * shades the band.
 */
export function Section({
  size = 'md',
  tone,
  className,
  containerClassName,
  children,
}: {
  size?: keyof typeof SECTION_SIZES;
  tone?: 'muted';
  className?: string;
  containerClassName?: string;
  children: ReactNode;
}) {
  return (
    <section
      className={cn(
        'border-b border-border last:border-b-0',
        tone === 'muted' && 'bg-muted/30',
        className,
      )}
    >
      <Container className={cn(SECTION_SIZES[size], containerClassName)}>
        {children}
      </Container>
    </section>
  );
}
