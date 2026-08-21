// Single attach point for SERVER-side error logging.
//
// The BFF's upstream catches return `502 upstreamUnreachable()` and NEVER echo an
// internal message to the client — good. But a bare `catch {}` also discards the
// error, so upstream outages/bugs are invisible in prod logs. `logServerError` is
// the server counterpart of the client hook `report-error.ts`: same structured,
// single-sink philosophy, but it writes to server stderr only (never the client).
//
// Server-only: import this from route handlers / BFF code, never from a Client
// Component (and never import the client `report-error` hook here). It carries no
// literal `server-only` marker so it stays unit-testable under `node --test`.

/** Where an error surfaced. Free-form tags, no PII/secrets. */
export interface ServerErrorContext {
  /** Route the error surfaced on, e.g. `'POST /api/engine/plan'`. */
  route?: string;
  /** Logical operation, e.g. `'engine.plan'`. */
  op?: string;
}

/** Normalized shape written to the server log. Deliberately free of PII/secrets. */
export interface ServerErrorReport {
  /** Human-readable error message (from the throwable, whatever its type). */
  message: string;
  /** Error class name, when the throwable is an Error. */
  name?: string;
  /** Route tag, when provided. */
  route?: string;
  /** Operation tag, when provided. */
  op?: string;
  /** Error stack, when available. Server-side only — never sent to the client. */
  stack?: string;
  /** ISO timestamp the report was created. */
  timestamp: string;
}

/**
 * Coerce any throwable into `{ message, name?, stack? }` without ever throwing.
 * Resilient to non-Error values (strings, objects, null, circular refs).
 */
function describe(error: unknown): { message: string; name?: string; stack?: string } {
  if (error instanceof Error) {
    return { message: error.message, name: error.name, stack: error.stack };
  }
  if (typeof error === 'string') {
    return { message: error };
  }
  try {
    const json = JSON.stringify(error);
    return { message: json ?? String(error) };
  } catch {
    // e.g. circular structure — fall back to a plain string cast.
    return { message: String(error) };
  }
}

/**
 * Build the structured report for `error`/`context`. Exported for unit tests.
 * Only tags and error metadata are included — no request body, tokens, or PII.
 */
export function buildServerErrorReport(
  error: unknown,
  context: ServerErrorContext = {},
): ServerErrorReport {
  const { message, name, stack } = describe(error);
  return {
    message,
    name,
    route: context.route,
    op: context.op,
    stack,
    timestamp: new Date().toISOString(),
  };
}

/**
 * Log a server-side error to stderr as a structured record. Call from a BFF
 * catch BEFORE returning the client-facing 502 — this is purely for operators.
 *
 * @param error   The caught throwable; may be any type.
 * @param context Optional `{ route, op }` tags identifying the call site.
 */
export function logServerError(error: unknown, context: ServerErrorContext = {}): void {
  const report = buildServerErrorReport(error, context);
  // Sanctioned server transport: stderr via console.error, never stdout-as-data
  // and never the client Response. Swap for a real sink (OTEL, Sentry) here.
  console.error('[amicu:server-error]', report);
}
