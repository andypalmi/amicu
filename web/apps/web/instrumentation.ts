import type { Instrumentation } from 'next';

// Sentry is only loaded when a DSN is configured; without one this file is inert.
const HAS_DSN = Boolean(process.env.NEXT_PUBLIC_SENTRY_DSN?.trim());

export async function register() {
  if (!HAS_DSN) return;
  if (process.env.NEXT_RUNTIME === 'nodejs' || process.env.NEXT_RUNTIME === 'edge') {
    await import('./sentry.server.config');
  }
}

// Server Components, route handlers and proxy errors. No-op without a DSN.
export const onRequestError: Instrumentation.onRequestError = async (...args) => {
  if (!HAS_DSN) return;
  const Sentry = await import('@sentry/nextjs');
  return Sentry.captureRequestError(...args);
};
