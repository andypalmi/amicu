import 'server-only';
import { createAuthClient } from '@amicu/auth/server';
import { API_URL } from './config';

// Single BFF-side auth client instance.
//
// TODO(amicu-planning#83): swap `createAuthClient` for the generated
// `@amicu/api-client` once the OpenAPI-derived client lands. This module is the
// only place the app constructs it, so the migration is contained here.
export const authClient = createAuthClient({ baseUrl: API_URL });
