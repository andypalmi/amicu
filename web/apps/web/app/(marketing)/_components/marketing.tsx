import type { ReactNode } from 'react';
import Link from 'next/link';
import { Button, cn } from '@amicu/ui';

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

/** Left-aligned heading for a section inside a page; `lead` is one short sentence. */
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
    <div className="max-w-2xl">
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
    <dl className={cn('border-b border-border', className)}>
      {items.map((item, i) => (
        <div
          key={i}
          className="grid gap-2 border-t border-border py-6 md:grid-cols-[minmax(0,5fr)_minmax(0,7fr)] md:gap-12"
        >
          <dt className="font-medium">{item.term}</dt>
          <dd className="max-w-prose text-pretty leading-relaxed text-muted-foreground">
            {item.detail}
          </dd>
        </div>
      ))}
    </dl>
  );
}

/** Bordered figure with a mono caption bar: code files and terminal output. */
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
    <figure
      className={cn(
        'flex min-w-0 flex-col overflow-hidden rounded-md border border-border bg-card',
        className,
      )}
    >
      <figcaption className="border-b border-border px-5 py-2.5 font-mono text-xs text-muted-foreground">
        {caption}
      </figcaption>
      {children}
    </figure>
  );
}

/** Flat, filled note for a state the reader should know about. */
export function StatusNote({ children, className }: { children: ReactNode; className?: string }) {
  return (
    <p
      className={cn(
        'rounded-md border border-border bg-muted px-4 py-3 text-sm text-muted-foreground',
        className,
      )}
    >
      {children}
    </p>
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
    <div className="flex flex-col items-start gap-6">
      <SectionHeader title={title} />
      {lede ? <p className="max-w-2xl text-pretty text-lg text-muted-foreground">{lede}</p> : null}
      <div className="flex flex-wrap items-center gap-3">
        {action(primary)}
        {secondary ? action(secondary, 'outline') : null}
      </div>
    </div>
  );
}

/**
 * A titled group of name/note entries that reflows into columns. The group
 * heading carries the meaning, so entries need no per-row badge.
 */
export function GroupedList({
  title,
  note,
  groups,
}: {
  title: string;
  note?: string;
  groups: { label?: string; entries: { name: string; note: string; detail?: string }[] }[];
}) {
  return (
    <section className="border-t border-border pt-8 first:border-t-0 first:pt-0">
      <h2 className="text-2xl font-semibold tracking-tight">{title}</h2>
      {note ? <p className="mt-2 text-muted-foreground">{note}</p> : null}
      <div className="mt-6 space-y-8">
        {groups.map((g) => (
          <div key={g.label ?? title}>
            {g.label ? <h3 className="mb-3 text-sm text-muted-foreground">{g.label}</h3> : null}
            <ul className="grid gap-x-10 gap-y-5 sm:grid-cols-2 lg:grid-cols-3">
              {g.entries.map((e) => (
                <li key={e.name} className="min-w-0">
                  <p className="font-medium">{e.name}</p>
                  {e.detail ? <p className="text-sm">{e.detail}</p> : null}
                  <p className="text-sm text-muted-foreground">{e.note}</p>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>
    </section>
  );
}
