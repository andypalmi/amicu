// Thin, typed runtime surface over the generated OpenAPI schema.
//
// This is the ONLY hand-written file in this package that touches the API shape,
// and it deliberately holds no business logic: it just wires `openapi-fetch` to a
// server-configured base URL. Everything it returns is typed straight from
// `./schema` (generated from `crates/amicu-schema/openapi.json`), so the client
// can never drift from the spec.
//
// Server-only intent: the base URL and any credentials carried on these calls must
// never reach the browser. Callers (the BFF) are responsible for keeping it that
// way — see `@amicu/auth/server`.

import createOpenApiClient, { type Client } from 'openapi-fetch';
import type { paths } from './schema';

export interface ApiClientOptions {
  /**
   * Base URL of the Rust Management API (server-only env, e.g. `API_URL`). Never a
   * `NEXT_PUBLIC_*` value — nothing here is safe to expose to the browser.
   */
  baseUrl: string;
  /** Optional `fetch` override (tests / instrumentation). */
  fetch?: typeof fetch;
}

/** A fully-typed Management API client, one method per HTTP verb (`GET`, `POST`, …). */
export type ApiClient = Client<paths>;

/**
 * Build a typed client bound to `baseUrl`. Trailing slashes are trimmed so callers
 * may pass either `https://api.example.com` or `https://api.example.com/`.
 */
export function createApiClient(options: ApiClientOptions): ApiClient {
  return createOpenApiClient<paths>({
    baseUrl: options.baseUrl.replace(/\/+$/, ''),
    fetch: options.fetch,
    // The BFF owns the session; never attach ambient browser cookies upstream.
    cache: 'no-store',
  });
}
