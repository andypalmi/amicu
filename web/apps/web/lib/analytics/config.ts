// Cookieless analytics configuration. Pure (no browser or posthog-js imports) so
// it is unit-testable under `node --test`. Everything is a no-op when the
// corresponding NEXT_PUBLIC_* variable is unset (dev, tests, current deploy).

export const POSTHOG_HOST = 'https://eu.i.posthog.com';
export const CF_BEACON_SRC = 'https://static.cloudflareinsights.com/beacon.min.js';

const clean = (v: string | undefined): string | null => v?.trim() || null;

/** Cloudflare Web Analytics token, or null (beacon not rendered). */
export function cfBeaconToken(value: string | undefined): string | null {
  return clean(value);
}

/** PostHog project key, or null (PostHog never initialised). */
export function posthogKey(value: string | undefined): string | null {
  return clean(value);
}

export interface PrivacySignals {
  /** navigator.doNotTrack ('1' / 'yes') */
  doNotTrack?: string | null;
  /** navigator.globalPrivacyControl */
  globalPrivacyControl?: boolean | null;
}

/** True when the visitor's browser asks not to be tracked (DNT or GPC). */
export function privacyOptOut(s: PrivacySignals): boolean {
  const dnt = s.doNotTrack;
  return s.globalPrivacyControl === true || dnt === '1' || dnt === 'yes';
}

/**
 * posthog-js options: no cookies/localStorage (memory persistence), no session
 * recording, no autocapture, no surveys or feature flags, DNT respected,
 * anonymous (no person profiles).
 */
export function posthogOptions() {
  return {
    api_host: POSTHOG_HOST,
    persistence: 'memory' as const,
    person_profiles: 'identified_only' as const,
    respect_dnt: true,
    disable_session_recording: true,
    autocapture: false,
    capture_pageview: false, // App Router pageviews are sent manually (components/analytics)
    capture_pageleave: false,
    capture_dead_clicks: false,
    capture_heatmaps: false,
    disable_surveys: true,
    advanced_disable_flags: true,
  };
}

/** Pageview URL without query string or fragment (tokens can live there). */
export function pageviewUrl(origin: string, pathname: string): string {
  return `${origin}${pathname}`;
}
