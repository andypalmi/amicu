'use client';

import type { ComponentProps, CSSProperties } from 'react';
import { useTheme } from 'next-themes';
import { Toaster as SonnerToaster } from 'sonner';

// The single mounted toast host for the app (see `app/layout.tsx`). It follows
// the same class-based theme fumadocs/next-themes toggles on `<html>` — reading
// the active theme from next-themes rather than the OS `prefers-color-scheme`
// so a toast matches whatever the in-app theme switch is set to.
//
// Colors are driven from the `@amicu/ui` design tokens (`tokens.css`) instead of
// being hardcoded, so toasts inherit the same surface/border/destructive palette
// as the rest of the UI and swap automatically in dark mode.
type ToasterProps = ComponentProps<typeof SonnerToaster>;

export function Toaster(props: ToasterProps) {
  const { theme = 'system' } = useTheme();

  return (
    <SonnerToaster
      theme={theme as ToasterProps['theme']}
      style={
        {
          '--normal-bg': 'var(--popover)',
          '--normal-text': 'var(--popover-foreground)',
          '--normal-border': 'var(--border)',
          '--error-bg': 'var(--popover)',
          '--error-text': 'var(--destructive)',
          '--error-border': 'var(--destructive)',
          '--border-radius': 'var(--radius)',
        } as CSSProperties
      }
      {...props}
    />
  );
}
