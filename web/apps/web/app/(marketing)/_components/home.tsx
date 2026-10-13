import type { ReactNode } from 'react';
import Link from 'next/link';
import { cn } from '@amicu/ui';
import type { DiffLine, FeatureLink } from '../_lib/landing';

// Building blocks for the Vercel-style home: one huge headline, one product
// snapshot, a two-tone proof line, and a short list of plain text links.

// Layout constants, defined once. Every section shares one container
// (max-w-6xl), one vertical rhythm and one text measure.
/** Cap for running text (sub-copy, proof lines, link lists). */
export const MEASURE = 'max-w-2xl';
/** Headline to content: 48px. */
export const GAP_HEAD = 'mt-12';
/** Content to proof line: 40px. */
export const GAP_PROOF = 'mt-10';
/** Proof line to links: 24px. */
export const GAP_LINKS = 'mt-6';

/** Full-width band: one container, 96px (64px on mobile) vertical padding. */
export function HomeSection({
  id,
  className,
  children,
}: {
  id?: string;
  className?: string;
  children: ReactNode;
}) {
  return (
    <section id={id} className="border-t border-border first:border-t-0">
      <div className={cn('mx-auto w-full max-w-6xl px-4 py-16 sm:px-6 sm:py-24', className)}>
        {children}
      </div>
    </section>
  );
}

/** The section headline: one size for every h2, the h1 one step larger. */
export function Headline({
  as: Tag = 'h2',
  className,
  children,
}: {
  as?: 'h1' | 'h2';
  className?: string;
  children: ReactNode;
}) {
  return (
    <Tag
      className={cn(
        'text-balance font-semibold tracking-[-0.05em] leading-[1.05]',
        Tag === 'h1'
          ? '[font-size:clamp(2.5rem,7vw,4.5rem)]'
          : '[font-size:clamp(2.25rem,6vw,3.5rem)]',
        className,
      )}
    >
      {children}
    </Tag>
  );
}

/** Mock window: hairline border, 12px radius, quiet title bar, mono body. */
export function MockWindow({
  title,
  action,
  children,
  className,
}: {
  title: string;
  /** Optional control rendered right-aligned in the title bar. */
  action?: ReactNode;
  children: ReactNode;
  className?: string;
}) {
  return (
    <figure
      className={cn(
        'min-w-0 overflow-hidden rounded-xl border border-border bg-background text-left',
        className,
      )}
    >
      <figcaption className="flex items-center justify-between gap-2 border-b border-border px-4 py-2.5 font-mono text-xs text-muted-foreground">
        <span>{title}</span>
        {action ? <span className="-my-1.5 -mr-1.5 flex">{action}</span> : null}
      </figcaption>
      {children}
    </figure>
  );
}

/** Two windows side by side from lg, stacked below; no overlap or offset. */
export function WindowPair({ left, right }: { left: ReactNode; right: ReactNode }) {
  return (
    <div className={cn('grid gap-6 lg:grid-cols-2', GAP_HEAD)}>
      {left}
      {right}
    </div>
  );
}

/** Spacing wrapper so a single snapshot sits the same distance under the headline. */
export function Snapshot({ children }: { children: ReactNode }) {
  return <div className={GAP_HEAD}>{children}</div>;
}

/** Two-tone statement: first clause in foreground, the rest muted. */
export function ProofLine({ lead, rest }: { lead: string; rest: string }) {
  return (
    <p
      className={cn(
        'text-balance text-2xl font-semibold tracking-[-0.03em] leading-[1.2] sm:text-[1.75rem]',
        GAP_PROOF,
        MEASURE,
      )}
    >
      <span>{lead}</span> <span className="text-muted-foreground">{rest}</span>
    </p>
  );
}

/** "Features" label and plain text links with hairline dividers. */
export function FeatureLinks({
  links,
  all,
}: {
  links: FeatureLink[];
  all?: FeatureLink;
}) {
  const items = all ? [...links, all] : links;
  return (
    <div className={cn(GAP_LINKS, MEASURE)}>
      <p className="mb-3 font-mono text-xs uppercase tracking-wider text-muted-foreground">
        Features
      </p>
      <ul className="border-b border-border">
        {items.map((l) => (
          <li key={l.href} className="border-t border-border">
            <Link
              href={l.href}
              className="group flex items-center justify-between py-3 text-base text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
            >
              {l.label}
              <span
                aria-hidden
                className="text-muted-foreground transition-transform motion-safe:group-hover:translate-x-1 group-hover:text-foreground"
              >
                →
              </span>
            </Link>
          </li>
        ))}
      </ul>
    </div>
  );
}

const diffClass: Record<DiffLine['tone'], string> = {
  add: 'text-success',
  del: 'text-destructive',
  ctx: 'text-muted-foreground',
};
const diffMark: Record<DiffLine['tone'], string> = { add: '+', del: '-', ctx: ' ' };

/** Small HCL diff; lines wrap rather than scroll. */
export function DiffBody({ lines, label }: { lines: DiffLine[]; label: string }) {
  return (
    <pre
      aria-label={label}
      className="px-4 py-4 font-mono text-xs leading-6 sm:text-[0.8125rem]"
    >
      <code className="block">
        {lines.map((l, i) => (
          <span key={i} className={cn('block whitespace-pre-wrap break-words', diffClass[l.tone])}>
            {diffMark[l.tone]} {l.text}
          </span>
        ))}
      </code>
    </pre>
  );
}

/** Large bordered tile: mono graphic, title, one line, link. */
export function FeatureTile({
  title,
  body,
  graphic,
  href,
}: {
  title: string;
  body: string;
  graphic: string[];
  href: string;
}) {
  return (
    <article className="flex min-w-0 flex-col overflow-hidden rounded-xl border border-border bg-background">
      <pre
        aria-hidden
        className="min-h-40 overflow-hidden whitespace-pre-wrap break-words border-b border-border p-5 font-mono text-xs leading-6 text-muted-foreground"
      >
        {graphic.join('\n')}
      </pre>
      <div className="flex flex-1 flex-col gap-2 p-5">
        <h3 className="text-xl font-semibold tracking-tight">{title}</h3>
        <p className="text-pretty text-muted-foreground">{body}</p>
        <Link
          href={href}
          className="mt-auto pt-3 text-sm font-medium text-foreground underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        >
          Learn more →
        </Link>
      </div>
    </article>
  );
}
