// Browser Sentry init (Next.js instrumentation-client hook). No-op unless
// NEXT_PUBLIC_SENTRY_DSN is set. No replay or tracing integrations are added.
import * as Sentry from '@sentry/nextjs';
import { buildSentryOptions } from '@/lib/observability/sentry-options';

const options = buildSentryOptions(process.env.NEXT_PUBLIC_SENTRY_DSN);

if (options.enabled) {
  Sentry.init(options);
}

export const onRouterTransitionStart = Sentry.captureRouterTransitionStart;
