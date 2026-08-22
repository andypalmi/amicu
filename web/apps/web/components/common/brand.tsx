import type { SVGProps } from 'react';
import { cn } from '@amicu/ui';
import { appName } from '@/lib/shared';

// Shared product brand mark. The app otherwise presents itself as a plain text
// wordmark (see `appName` / the fumadocs nav title), so the mark here is the
// first real logo the product has. It is deliberately monochrome: the design
// token system is pure grayscale with NO brand accent color, so the mark draws
// with `currentColor` and inherits the surrounding foreground in either theme.
//
// Inline SVG only (strict CSP forbids external images/fonts). Reusable: import
// `BrandMark` for the glyph alone, or `Brand` for the mark + wordmark lockup.

/**
 * The Amicu monogram: an outline "A" set in a rounded square, drawn in
 * `currentColor`. Decorative by default (`aria-hidden`) since the wordmark
 * carries the accessible name in the `Brand` lockup; pass a `title`/role via
 * props when used on its own.
 */
export function BrandMark({ className, ...props }: SVGProps<SVGSVGElement>) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.75}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={cn('h-6 w-6', className)}
      aria-hidden
      {...props}
    >
      <rect x={2.5} y={2.5} width={19} height={19} rx={5.5} />
      <path d="M7.5 16.5 12 7l4.5 9.5" />
      <path d="M9.25 13h5.5" />
    </svg>
  );
}

export interface BrandProps {
  /** Size of the monogram; the wordmark scales with the surrounding text. */
  className?: string;
  /** Additional classes for the mark glyph. */
  markClassName?: string;
  /** Hide the wordmark and render the monogram only. */
  markOnly?: boolean;
}

/**
 * Brand lockup: monogram + "Amicu" wordmark. Neutral, token-driven, and safe to
 * drop anywhere the current foreground color is appropriate (auth surface,
 * headers, footers). Render inside a `<Link>` or `<a>` when it should navigate.
 */
export function Brand({ className, markClassName, markOnly = false }: BrandProps) {
  return (
    <span
      className={cn('inline-flex items-center gap-2 tracking-tight', className)}
    >
      <BrandMark className={markClassName} />
      {markOnly ? (
        <span className="sr-only">{appName}</span>
      ) : (
        <span className="text-lg font-semibold leading-none">{appName}</span>
      )}
    </span>
  );
}
