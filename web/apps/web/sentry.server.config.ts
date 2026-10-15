// Server (Node + edge) Sentry init, loaded from instrumentation.ts. No-op unless
// NEXT_PUBLIC_SENTRY_DSN is set. See lib/observability/sentry-options.ts.
import * as Sentry from '@sentry/nextjs';
import { buildSentryOptions } from '@/lib/observability/sentry-options';
import { registerServerErrorSink } from '@/lib/observability/log-server-error';

const options = buildSentryOptions(process.env.NEXT_PUBLIC_SENTRY_DSN);

if (options.enabled) {
  Sentry.init(options);
  // Errors the BFF catches and turns into a 502 would otherwise never reach Sentry.
  registerServerErrorSink((error, context) => {
    Sentry.captureException(error, { tags: { route: context.route, op: context.op } });
  });
}
