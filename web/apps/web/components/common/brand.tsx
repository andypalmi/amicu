import { cn } from '@amicu/ui';

// Shared product logo: the typed lowercase "amicu" wordmark, set in the
// foreground colour so it follows light and dark themes. The marketing header,
// footer and auth screens all render this one component.

export interface BrandProps {
  /** Size and spacing overrides; the wordmark scales with the font size. */
  className?: string;
}

export function Brand({ className }: BrandProps) {
  return (
    <span className={cn('text-xl font-semibold leading-none tracking-tight', className)}>
      amicu
    </span>
  );
}
