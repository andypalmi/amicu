import * as Sentry from '@sentry/nextjs';

export async function register() {
  if (process.env.NEXT_RUNTIME === 'nodejs' || process.env.NEXT_RUNTIME === 'edge') {
    await import('./sentry.server.config');
  }
}

// Server Components, route handlers and proxy errors. Inert when Sentry is not
// initialised (no DSN).
export const onRequestError = Sentry.captureRequestError;
