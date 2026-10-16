import { createMDX } from 'fumadocs-mdx/next';

const withMDX = createMDX();

const siteHost = (() => {
  try {
    return process.env.NEXT_PUBLIC_SITE_URL ? new URL(process.env.NEXT_PUBLIC_SITE_URL).host : null;
  } catch {
    return null;
  }
})();

const appHost = (() => {
  try {
    return process.env.NEXT_PUBLIC_APP_URL ? new URL(process.env.NEXT_PUBLIC_APP_URL).host : null;
  } catch {
    return null;
  }
})();

const isProd = process.env.NODE_ENV === 'production';

// Content-Security-Policy for the whole app.
//
// Pragmatic (non-nonce) approach: `script-src` allows 'unsafe-inline' because
// the Next.js App Router injects inline bootstrap/RSC-streaming scripts
// (self.__next_f.push(...)) that carry no nonce or hash. A per-request nonce is
// the stricter alternative, but it must be threaded through middleware and read
// back from the request CSP header; our proxy.ts already runs auth-refresh on
// /api/* routes and rewrites docs requests, and Next warns the nonce disables
// caching of prefetched RSC payloads - reconciling that with the existing
// middleware branches (and verifying it produces no runtime CSP violations
// across the fumadocs docs area, Tailwind v4 and next/font) was not safely
// achievable without browser verification. We deliberately avoid 'unsafe-eval'.
//
// Notes on the host allowlists:
//  - Fonts are self-hosted by next/font/google (downloaded at build time and
//    served same-origin), so no fonts.googleapis.com / fonts.gstatic.com hosts
//    are needed - `font-src 'self'` and `style-src 'self'` suffice.
//  - The browser only ever calls same-origin /api/* (BFF pattern); the server
//    talks to the upstream API. So `connect-src 'self'` is correct and the
//    upstream origin (https://api.amicu.dev in production) is intentionally
//    NOT listed.
// Cloudflare Turnstile (signup / forgot / resend-verification). Scripts, the
// challenge iframe and its XHR all come from this single origin.
const turnstileOrigin = 'https://challenges.cloudflare.com';

// Observability origins, all conditional on build-time env so a build without
// them keeps the original strict policy. Cloudflare Web Analytics beacon, PostHog
// (EU) and the Sentry ingest host (derived from the public DSN).
const cfBeaconOrigins = process.env.NEXT_PUBLIC_CF_BEACON_TOKEN
  ? ['https://static.cloudflareinsights.com', 'https://cloudflareinsights.com']
  : [];
const posthogOrigins = process.env.NEXT_PUBLIC_POSTHOG_KEY
  ? ['https://eu.i.posthog.com', 'https://eu-assets.i.posthog.com']
  : [];
const sentryOrigins = (() => {
  try {
    return process.env.NEXT_PUBLIC_SENTRY_DSN ? [new URL(process.env.NEXT_PUBLIC_SENTRY_DSN).origin] : [];
  } catch {
    return [];
  }
})();

const cspDirectives = [
  "default-src 'self'",
  // 'unsafe-eval' is added in dev only: React 19's Fast Refresh runtime and the
  // Next.js error overlay use eval() in the browser. Production stays strict and
  // must NOT include 'unsafe-eval'. Matches Next.js's official CSP guidance.
  `script-src 'self' 'unsafe-inline' ${[turnstileOrigin, ...cfBeaconOrigins, ...posthogOrigins].join(' ')}${isProd ? '' : " 'unsafe-eval'"}`,
  "style-src 'self' 'unsafe-inline'",
  "font-src 'self'",
  "img-src 'self' blob: data:",
  `connect-src 'self' ${[turnstileOrigin, ...cfBeaconOrigins, ...posthogOrigins, ...sentryOrigins].join(' ')}`,
  // Turnstile renders its challenge in an iframe.
  `frame-src ${turnstileOrigin}`,
  "object-src 'none'",
  "base-uri 'self'",
  "frame-ancestors 'none'",
  "form-action 'self'",
];

const contentSecurityPolicy = cspDirectives.join('; ');

const securityHeaders = [
  { key: 'Content-Security-Policy', value: contentSecurityPolicy },
  { key: 'X-Frame-Options', value: 'DENY' },
  { key: 'X-Content-Type-Options', value: 'nosniff' },
  { key: 'Referrer-Policy', value: 'strict-origin-when-cross-origin' },
  { key: 'Permissions-Policy', value: 'camera=(), microphone=(), geolocation=()' },
  // HSTS is production-only: a dev build over plain HTTP must not pin the host.
  ...(isProd
    ? [
        {
          key: 'Strict-Transport-Security',
          value: 'max-age=63072000; includeSubDomains; preload',
        },
      ]
    : []),
];

/** @type {import('next').NextConfig} */
const config = {
  reactStrictMode: true,
  // Server actions already reject cross-origin calls (Origin vs Host /
  // X-Forwarded-Host). Behind Cloudflare the public host may differ from what the
  // Worker sees, so also allow the configured public site host explicitly.
  experimental: {
    serverActions: { allowedOrigins: [siteHost, appHost].filter(Boolean) },
  },
  transpilePackages: ['@amicu/ui', '@amicu/auth'],
  async redirects() {
    // One-line installers (shell and PowerShell). Config redirects run before proxy.ts. `releases/latest`
    // skips prereleases, so this serves the newest stable release.
    return [
      {
        source: '/install.sh',
        destination:
          'https://github.com/andypalmi/amicu/releases/latest/download/amicu-cli-installer.sh',
        statusCode: 302,
      },
      {
        source: '/install.ps1',
        destination:
          'https://github.com/andypalmi/amicu/releases/latest/download/amicu-cli-installer.ps1',
        statusCode: 302,
      },
    ];
  },
  async headers() {
    return [
      {
        // Apply the security headers to every route.
        source: '/:path*',
        headers: securityHeaders,
      },
    ];
  },
};

export default withMDX(config);
