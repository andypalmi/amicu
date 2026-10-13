import type { CSSProperties } from 'react';
import { highlight } from 'fumadocs-core/highlight';
import { cn } from '@amicu/ui';
import type { TermLine } from '../_lib/landing';

// Server-rendered HCL highlighting via Fumadocs' Shiki wrapper. The theme is
// a two-tone scheme built only from semantic design tokens (foreground for
// structure and values, muted-foreground for keys, operators and comments),
// so it follows light/dark and any palette change without its own colours.
const fg = 'var(--color-foreground)';
const muted = 'var(--color-muted-foreground)';

const tokenTheme = {
  name: 'amicu-tokens',
  type: 'light' as const,
  fg,
  bg: 'transparent',
  settings: [
    { settings: { foreground: fg } },
    { scope: ['entity.name.type'], settings: { foreground: fg, fontStyle: 'bold' } },
    { scope: ['variable.other.readwrite', 'keyword.operator', 'punctuation'], settings: { foreground: muted } },
    { scope: ['comment', 'punctuation.definition.comment'], settings: { foreground: muted, fontStyle: 'italic' } },
    { scope: ['string', 'constant', 'variable.other.enummember', 'variable.other.member'], settings: { foreground: fg } },
  ],
};

/** Highlighted HCL, rendered on the server (no client JS). */
export async function HclCode({ code, className }: { code: string; className?: string }) {
  const rendered = await highlight(code, { lang: 'hcl', theme: tokenTheme });
  return (
    <div
      className={cn(
        'font-mono text-[0.8125rem] leading-6 [&_pre]:overflow-x-auto [&_pre]:outline-none',
        className,
      )}
    >
      {rendered}
    </div>
  );
}

const toneClass: Record<NonNullable<TermLine['tone']> | 'plain', string> = {
  cmd: 'text-foreground font-semibold',
  add: 'text-success',
  error: 'text-destructive',
  muted: 'text-muted-foreground',
  plain: 'text-foreground',
};

/**
 * Terminal transcript. With `reveal`, lines fade in one after another on
 * first paint (CSS `@starting-style` via Tailwind's `starting:` variant, so
 * no JS); `motion-safe:` keeps it static under prefers-reduced-motion.
 */
export function Transcript({
  lines,
  reveal = false,
  wrap = false,
  className,
  label,
}: {
  lines: TermLine[];
  reveal?: boolean;
  /** Wrap long lines instead of scrolling horizontally. */
  wrap?: boolean;
  className?: string;
  label: string;
}) {
  return (
    <pre
      tabIndex={0}
      aria-label={label}
      className={cn(
        'overflow-x-auto font-mono text-[0.8125rem] leading-6 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring',
        className,
      )}
    >
      <code className={cn('block', !wrap && 'min-w-max')}>
        {lines.map((line, i) => (
          <span
            // Lines are static content; index keys are stable.
            key={i}
            className={cn(
              wrap ? 'block whitespace-pre-wrap break-words' : 'block whitespace-pre',
              toneClass[line.tone ?? 'plain'],
              reveal &&
                'motion-safe:transition-opacity motion-safe:duration-500 motion-safe:ease-out motion-safe:starting:opacity-0',
            )}
            style={reveal ? ({ transitionDelay: `${300 + i * 140}ms` } as CSSProperties) : undefined}
          >
            {line.text || ' '}
          </span>
        ))}
      </code>
    </pre>
  );
}
