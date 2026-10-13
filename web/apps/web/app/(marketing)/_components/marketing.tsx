import type { ReactNode } from 'react';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';
import { CopyButton } from './copy-button';

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
export function PageHeader({ title, description }: { title: string; description?: string }) {
  return (
    <header className="mx-auto max-w-3xl text-center">
      <h1 className="text-balance text-4xl font-bold tracking-tight sm:text-5xl">
        {title}
      </h1>
      {description ? (
        <p className="mx-auto mt-4 max-w-2xl text-pretty text-lg text-muted-foreground">
          {description}
        </p>
      ) : null}
    </header>
  );
}

/** Full-width marketing band, ruled from the band above, with consistent vertical rhythm. */
export function Section({
  className,
  containerClassName,
  children,
}: {
  className?: string;
  containerClassName?: string;
  children: ReactNode;
}) {
  return (
    <section className={cn('border-t border-border first:border-t-0', className)}>
      <Container className={cn('py-20 sm:py-28', containerClassName)}>{children}</Container>
    </section>
  );
}

/** Centered heading for a section inside a page; `lead` is one short sentence. */
export function SectionHeader({
  id,
  title,
  lead,
}: {
  id?: string;
  title: string;
  lead?: string;
}) {
  return (
    <div className="mx-auto max-w-2xl text-center">
      <h2
        id={id}
        className="scroll-mt-24 text-balance text-3xl font-semibold tracking-tight sm:text-4xl"
      >
        {title}
      </h2>
      {lead ? <p className="mt-3 text-pretty text-lg text-muted-foreground">{lead}</p> : null}
    </div>
  );
}

/**
 * Ruled term/detail rows: the one list pattern for outcomes, support facts,
 * security pillars and providers.
 */
export function FactList({
  items,
  className,
}: {
  items: { term: ReactNode; detail: ReactNode }[];
  className?: string;
}) {
  return (
    <dl className={cn('mx-auto max-w-3xl border-b border-border text-center', className)}>
      {items.map((item, i) => (
        <div key={i} className="flex flex-col gap-2 border-t border-border py-6">
          <dt className="font-medium">{item.term}</dt>
          <dd className="mx-auto max-w-prose text-pretty leading-relaxed text-muted-foreground">
            {item.detail}
          </dd>
        </div>
      ))}
    </dl>
  );
}

/** Same frame as a docs code block: caption bar, copy button, shared Shiki theme. */
export function CodePanel({
  caption,
  children,
  className,
}: {
  caption: string;
  children: ReactNode;
  className?: string;
}) {
  return (
    <figure className={cn('code-frame flex min-w-0 flex-col', className)}>
      <figcaption className="code-bar">
        <span className="flex-1 truncate">{caption}</span>
        <CopyButton />
      </figcaption>
      {children}
    </figure>
  );
}

/** Closing call to action: one short sentence and two actions. */
export function CTABand({
  title,
  lede,
  primary,
  secondary,
}: {
  title: string;
  lede?: ReactNode;
  primary: { label: string; href: string; external?: boolean };
  secondary?: { label: string; href: string; external?: boolean };
}) {
  const action = (a: { label: string; href: string; external?: boolean }, variant?: 'outline') => (
    <Button asChild size="lg" variant={variant}>
      {a.external ? <a href={a.href}>{a.label}</a> : <Link href={a.href}>{a.label}</Link>}
    </Button>
  );
  return (
    <div className="flex flex-col items-center gap-6 text-center">
      <SectionHeader title={title} />
      {lede ? <p className="mx-auto max-w-2xl text-pretty text-lg text-muted-foreground">{lede}</p> : null}
      <div className="flex flex-wrap items-center justify-center gap-3">
        {action(primary)}
        {secondary ? action(secondary, 'outline') : null}
      </div>
    </div>
  );
}
