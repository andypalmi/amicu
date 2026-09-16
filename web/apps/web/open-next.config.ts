// OpenNext configuration for the Cloudflare Workers adapter.
//
// This is the minimal, non-overridden config: no R2 incremental cache, no
// Durable Object tag cache/queue, no multi-worker split. The amicu web app is
// a pure BFF (server-only route handlers/actions calling the Rust API) with
// no ISR/on-demand revalidation requirements today, so the defaults
// (in-memory-per-isolate incremental cache, no queue) are sufficient. Revisit
// if the app grows ISR/tag-revalidation needs - see
// https://opennext.js.org/cloudflare/howtos for the R2/DO overrides.
import { defineCloudflareConfig } from '@opennextjs/cloudflare';

export default defineCloudflareConfig();
