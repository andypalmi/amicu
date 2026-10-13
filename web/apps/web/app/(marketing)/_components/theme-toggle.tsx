'use client';

import { Moon, Sun } from 'lucide-react';
import { useTheme } from 'next-themes';

// Light/dark switch for the marketing header. Both icons are always rendered
// and swapped with the `dark:` variant, so server and client markup match and
// there is no hydration flash; only the click handler needs JS.
export function ThemeToggle() {
  const { resolvedTheme, setTheme } = useTheme();
  return (
    <button
      type="button"
      onClick={() => setTheme(resolvedTheme === 'dark' ? 'light' : 'dark')}
      className="inline-flex size-9 items-center justify-center rounded-md text-muted-foreground transition-colors hover:bg-accent hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
      aria-label="Toggle colour theme"
    >
      <Sun className="hidden size-4 dark:block" aria-hidden />
      <Moon className="size-4 dark:hidden" aria-hidden />
    </button>
  );
}
