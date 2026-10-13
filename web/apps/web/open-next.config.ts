// OpenNext configuration for the Cloudflare Workers adapter.
//
// Incremental cache: Workers static assets (read-only). `next build` prerenders
// every docs page (generateStaticParams) and the marketing pages, but with
// OpenNext's default (a dummy cache) those prerendered entries are never read:
// each request, including every RSC prefetch fired by the docs sidebar, re-ran
// the full React/MDX render inside the Worker (~0.6s CPU median, 1.0-1.5s under
// a burst), which makes bursts of prefetches exhaust Worker resources and
// return 503. The static-assets cache serves the build-time prerender output
// instead. It is read-only, which fits: the app has no ISR/on-demand
// revalidation or tag invalidation (a pure BFF over the Rust API). If ISR or
// revalidateTag/revalidatePath are ever needed, switch to the R2 or KV cache and
// add a tag cache - see https://opennext.js.org/cloudflare/caching
//
// `pnpm cf:build` copies the prerender output into .open-next/assets/cdn-cgi/
// (`populateCache local`, no credentials needed), so a plain `wrangler deploy`
// ships it; `opennextjs-cloudflare deploy|preview|upload` repeat the same
// idempotent local copy.
import { defineCloudflareConfig } from '@opennextjs/cloudflare';
import staticAssetsIncrementalCache from '@opennextjs/cloudflare/overrides/incremental-cache/static-assets-incremental-cache';

export default defineCloudflareConfig({
  incrementalCache: staticAssetsIncrementalCache,
});
