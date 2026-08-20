import 'server-only';
import { createAuthClient } from '@amicu/auth/server';
import { API_URL } from './config';

// Single BFF-side auth client instance.
//
// Resolved amicu-planning#83: `createAuthClient` is now a thin ergonomic surface
// over the generated `@amicu/api-client` (OpenAPI-derived), so its request/response
// shapes come straight from the spec. This module stays the only place the app
// constructs it, keeping the wiring contained.
export const authClient = createAuthClient({ baseUrl: API_URL });
