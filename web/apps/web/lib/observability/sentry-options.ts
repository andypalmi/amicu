// Privacy-first Sentry configuration shared by the browser, Node and edge
// runtimes. Errors only: no tracing, no session replay, no default PII.
//
// Pure (no @sentry/* import) so it is unit-testable under `node --test`.
// The DSN is public by design; unset => Sentry is fully disabled (no network).
//
// TODO: source-map upload (withSentryConfig + SENTRY_AUTH_TOKEN in CI) is
// deliberately not wired here. Without it stack traces are minified.

/** Minimal structural view of a Sentry event; only what we scrub. */
export interface ScrubbableEvent {
  user?: unknown;
  request?: {
    url?: string;
    method?: string;
    headers?: unknown;
    cookies?: unknown;
    data?: unknown;
    query_string?: unknown;
  };
  breadcrumbs?: unknown;
  server_name?: unknown;
}

/** Remove the query string and fragment from a URL (tokens live there, e.g. /verify?token=). */
export function stripUrlQuery(url: string): string {
  const cut = url.search(/[?#]/);
  return cut === -1 ? url : url.slice(0, cut);
}

/**
 * Strip everything personal from an event before it leaves the process: user,
 * IP, request body/cookies/headers/query string, breadcrumbs and server name.
 * Keeps the error, stack, URL path and method.
 */
export function scrubEvent<T extends ScrubbableEvent>(event: T): T {
  delete event.user;
  delete event.breadcrumbs;
  delete event.server_name;
  if (event.request) {
    const { url, method } = event.request;
    event.request = {
      ...(url ? { url: stripUrlQuery(url) } : {}),
      ...(method ? { method } : {}),
    };
  }
  return event;
}

export interface SentryBaseOptions {
  dsn: string | undefined;
  enabled: boolean;
  sendDefaultPii: false;
  tracesSampleRate: 0;
  sampleRate: 1;
  maxBreadcrumbs: 0;
  beforeSend: <T extends ScrubbableEvent>(event: T) => T | null;
  beforeBreadcrumb: () => null;
}

export function buildSentryOptions(dsn: string | undefined): SentryBaseOptions {
  const trimmed = dsn?.trim() || undefined;
  return {
    dsn: trimmed,
    enabled: Boolean(trimmed),
    sendDefaultPii: false,
    tracesSampleRate: 0,
    sampleRate: 1,
    maxBreadcrumbs: 0,
    beforeSend: scrubEvent,
    beforeBreadcrumb: () => null,
  };
}
