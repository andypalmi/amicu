'use client';

import { useEffect } from 'react';
import { tokens } from '@/lib/generated/tokens';
import { siteHref } from '@/lib/shared';
import { reportClientError } from '@/lib/observability/report-error';

// Root error boundary for CATASTROPHIC failures - thrown while rendering the
// root layout itself. It replaces the whole document, so it must supply its own
// <html>/<body> and cannot rely on the root layout or the app's global CSS
// (which is imported by that layout). Styling is therefore self-contained and
// theme-aware via a scoped <style> block using the amicu design tokens.
export default function GlobalError({
  error,
  reset,
}: {
  error: Error & { digest?: string };
  reset: () => void;
}) {
  useEffect(() => {
    reportClientError(error, 'global-error');
  }, [error]);

  return (
    <html lang="en">
      <body>
        <style>{globalErrorStyles}</style>
        <main className="amicu-ge">
          <div className="amicu-ge-card" role="alert">
            <p className="amicu-ge-eyebrow">amicu</p>
            <h1 className="amicu-ge-title">Something went badly wrong</h1>
            <p className="amicu-ge-body">
              The application hit an unrecoverable error while loading. Try reloading -
              if the problem persists, please try again in a little while.
            </p>
            {error.digest ? (
              <p className="amicu-ge-ref">Reference: {error.digest}</p>
            ) : null}
            <div className="amicu-ge-actions">
              <button type="button" className="amicu-ge-btn" onClick={reset}>
                Try again
              </button>
              {/* Intentional full-document reload: the router tree that <Link>
                  relies on is what just failed, so a hard navigation is safer. */}
              <a className="amicu-ge-btn amicu-ge-btn--ghost" href={siteHref('/')}>
                Go home
              </a>
            </div>
          </div>
        </main>
      </body>
    </html>
  );
}

// Built from the design tokens (generated from packages/ui tokens.css, since
// the app stylesheet is unavailable here). Light + dark via prefers-color-scheme.
// Light: primary + its foreground. Dark: the lighter accent-text orange with its
// dark foreground (keeps the original light-orange-with-dark-text look, AA).
const geVars = (t: typeof tokens.light | typeof tokens.dark, btn: readonly [string, string]) => `
    --ge-bg: ${t.background};
    --ge-fg: ${t.foreground};
    --ge-card: ${t.card};
    --ge-muted: ${t.mutedForeground};
    --ge-border: ${t.border};
    --ge-primary: ${btn[0]};
    --ge-primary-fg: ${btn[1]};
    --ge-accent: ${t.accent};`;
const globalErrorStyles = `
  :root {${geVars(tokens.light, [tokens.light.primary, tokens.light.primaryForeground])}
  }
  @media (prefers-color-scheme: dark) {
    :root {${geVars(tokens.dark, [tokens.dark.accentText, tokens.dark.accentTextForeground])}
    }
  }
  :root { color-scheme: light dark; }
  * { box-sizing: border-box; }
  body {
    margin: 0;
    background: var(--ge-bg);
    color: var(--ge-fg);
    font-family: ui-sans-serif, system-ui, -apple-system, "Segoe UI", Roboto,
      Helvetica, Arial, sans-serif;
  }
  .amicu-ge {
    min-height: 100vh;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 1.5rem;
  }
  .amicu-ge-card {
    width: 100%;
    max-width: 32rem;
    background: var(--ge-card);
    border: 1.5px solid var(--ge-border);
    border-radius: 4px;
    padding: 2rem;
  }
  .amicu-ge-eyebrow {
    margin: 0 0 0.75rem;
    font-size: 0.75rem;
    font-weight: 600;
    color: var(--ge-muted);
  }
  .amicu-ge-title {
    margin: 0 0 0.75rem;
    font-size: 1.5rem;
    line-height: 1.2;
    font-weight: 700;
    letter-spacing: -0.01em;
  }
  .amicu-ge-body {
    margin: 0;
    color: var(--ge-muted);
    line-height: 1.6;
  }
  .amicu-ge-ref {
    margin: 0.75rem 0 0;
    font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
    font-size: 0.75rem;
    color: var(--ge-muted);
  }
  .amicu-ge-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 0.75rem;
    margin-top: 1.5rem;
  }
  .amicu-ge-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    height: 2.25rem;
    padding: 0 1rem;
    border: 1px solid transparent;
    border-radius: 3px;
    font-size: 0.875rem;
    font-weight: 500;
    text-decoration: none;
    cursor: pointer;
    background: var(--ge-primary);
    color: var(--ge-primary-fg);
  }
  .amicu-ge-btn--ghost {
    background: transparent;
    color: var(--ge-fg);
    border-color: var(--ge-border);
  }
  .amicu-ge-btn:hover { opacity: 0.9; }
`;
