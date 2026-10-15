'use client';

import { Suspense, useEffect } from 'react';
import { usePathname } from 'next/navigation';
import posthog from 'posthog-js';
import {
  pageviewUrl,
  posthogKey,
  posthogOptions,
  privacyOptOut,
} from '@/lib/analytics/config';

// Inlined at build time. Unset => nothing below ever runs.
const KEY = posthogKey(process.env.NEXT_PUBLIC_POSTHOG_KEY);

let started = false;

function start(): boolean {
  if (!KEY) return false;
  if (started) return true;
  const nav = navigator as Navigator & { globalPrivacyControl?: boolean };
  if (privacyOptOut({ doNotTrack: nav.doNotTrack, globalPrivacyControl: nav.globalPrivacyControl })) {
    return false;
  }
  posthog.init(KEY, posthogOptions());
  started = true;
  return true;
}

function PageViews() {
  const pathname = usePathname();
  useEffect(() => {
    if (!start()) return;
    // Path only: never the query string or fragment.
    posthog.capture('$pageview', { $current_url: pageviewUrl(window.location.origin, pathname) });
  }, [pathname]);
  return null;
}

/** Cookieless PostHog pageviews for the App Router. Renders nothing; inert without a key. */
export function Analytics() {
  if (!KEY) return null;
  return (
    <Suspense fallback={null}>
      <PageViews />
    </Suspense>
  );
}
