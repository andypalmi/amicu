// Offline access-token verification against the TASK-078 JWKS (RFC 0001 §6, §11.6).
// Server-only: uses `jose` and network access to the issuer's JWKS endpoint.

import { createRemoteJWKSet, jwtVerify, decodeJwt, type JWTPayload } from 'jose';
import { normalizeClaims, type AccessClaims } from './claims';

export interface VerifierOptions {
  /** Absolute URL of the issuer JWKS, e.g. `${API_URL}/.well-known/jwks.json`. */
  jwksUrl: string;
  /** Expected `iss`. When set, tokens with a different issuer are rejected. */
  issuer?: string;
  /** Expected `aud`. When set, tokens with a different audience are rejected. */
  audience?: string;
}

// JWKS sets are cached per URL (jose caches keys + honors HTTP cache headers and
// refetches on an unknown `kid`, matching the TASK-078 rotation runbook).
const jwksCache = new Map<string, ReturnType<typeof createRemoteJWKSet>>();

function jwks(url: string) {
  let set = jwksCache.get(url);
  if (!set) {
    set = createRemoteJWKSet(new URL(url));
    jwksCache.set(url, set);
  }
  return set;
}

/**
 * Verify an access token's signature, expiry, issuer and audience against the
 * issuer JWKS, then project it into typed claims. Returns null on any failure
 * (invalid signature, expired, wrong aud/iss, malformed claims).
 */
export async function verifyAccessToken(
  token: string,
  options: VerifierOptions,
): Promise<AccessClaims | null> {
  try {
    const { payload } = await jwtVerify(token, jwks(options.jwksUrl), {
      issuer: options.issuer,
      audience: options.audience,
    });
    return normalizeClaims(payload as JWTPayload as Record<string, unknown>);
  } catch {
    return null;
  }
}

/**
 * Read the `exp` from a token WITHOUT verifying its signature. Only for the cheap
 * "should the BFF proactively refresh this?" decision in middleware — never trust
 * the result for anything security-relevant. Returns seconds-since-epoch or null.
 */
export function decodeExpUnsafe(token: string): number | null {
  try {
    const exp = decodeJwt(token).exp;
    return typeof exp === 'number' ? exp : null;
  } catch {
    return null;
  }
}
