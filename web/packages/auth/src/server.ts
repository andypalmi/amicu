// @amicu/auth/server — server-only entry point.
//
// Everything here depends on `jose`, network access, or credentials and must only
// run in the BFF (server actions / route handlers / middleware). Never import this
// from a client component.

export {
  verifyAccessToken,
  decodeExpUnsafe,
  type VerifierOptions,
} from './jwt';

export {
  createAuthClient,
  type AuthClient,
  type AuthClientOptions,
  type TokenResponse,
  type AuthResult,
} from './client';
