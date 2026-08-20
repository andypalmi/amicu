import 'server-only';
import { createApiClient, type ApiClient } from '@amicu/api-client';
import { API_URL } from '@/lib/auth/config';

// One process-wide Management API client for the engine BFF routes. Bound to the
// server-only `API_URL`; the base URL and any bearer token attached per call must
// never reach the browser — the browser talks only to the /api/engine/* handlers.
let client: ApiClient | undefined;

/** The shared, server-only Management API client. */
export function engineClient(): ApiClient {
  client ??= createApiClient({ baseUrl: API_URL });
  return client;
}
