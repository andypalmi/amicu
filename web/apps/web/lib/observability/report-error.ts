// Single attach point for client-side error reporting.
//
// Every App Router error boundary funnels through `reportClientError` so that
// future telemetry (see amicu-planning#108, where the BFF grows a logging
// endpoint) has exactly ONE place to converge on. For now this is a structured
// `console.error` stub — swap the body for a real transport (BFF beacon, Sentry,
// etc.) without touching any call site.

/** Normalized shape a transport can serialize. Kept generic on purpose. */
export interface ClientErrorReport {
  /** Human-readable error message. */
  message: string;
  /** Next.js production error digest, when present. */
  digest?: string;
  /** Free-form tag identifying where the error surfaced. */
  context?: string;
  /** Error stack, when available (omitted in production builds by React). */
  stack?: string;
  /** ISO timestamp the report was created. */
  timestamp: string;
}

/**
 * Report a client render error. Call from an error boundary's `useEffect`.
 *
 * @param error   The error React handed the boundary; may carry a Next.js `digest`.
 * @param context Optional source tag, e.g. `'route-error'` or `'global-error'`.
 */
export function reportClientError(
  error: Error & { digest?: string },
  context?: string,
): void {
  const report: ClientErrorReport = {
    message: error.message,
    digest: error.digest,
    context,
    stack: error.stack,
    timestamp: new Date().toISOString(),
  };

  // Stub transport. This is the single sanctioned `console.*` in the app; when
  // #108 lands, replace this line with a BFF beacon and keep the same shape.
  console.error('[amicu:client-error]', report);
}
