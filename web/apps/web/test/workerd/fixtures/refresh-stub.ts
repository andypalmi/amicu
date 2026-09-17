// A faithful-enough stand-in for the real `POST /token/refresh` semantics in
// crates/amicu-auth/src/service.rs (`AuthService::refresh`) and
// crates/amicu-store/src/refresh.rs (`PostgresRefreshStore::consume` /
// `compromise_family`), for tests that need to observe (not just assert away)
// how the BFF's silent-refresh path behaves under REUSE detection - not a
// stub that always mints a fresh pair regardless of input.
//
// Real semantics reproduced here:
//   - A refresh token is single-use, scoped to a `family_id` that survives
//     rotation (service.rs `issue_pair_in_family_for_workspace`).
//   - Presenting an unknown token -> 401 `{ "error": "refresh token invalid" }`
//     (AuthError::RefreshInvalid, crates/amicu-api/src/auth.rs ApiError
//     mapping - InvalidCredentials/RefreshInvalid both map to 401).
//   - Presenting an ALREADY-CONSUMED token (or one in an already-compromised
//     family) -> reuse/theft detection: the ENTIRE family is revoked
//     (refresh.rs `consume`'s `Reuse` branch -> service.rs calls
//     `compromise_family`, which revokes every live token in the family, not
//     just the replayed one) and the SAME 401 body is returned. Critically,
//     this means a token that was validly rotated moments earlier - if it
//     shares a family with a now-replayed token - becomes invalid too.
//   - A winning presentation consumes the token and mints a new pair in the
//     same family.
//
// This is intentionally NOT a full reimplementation (no expiry, no per-user
// scoping) - just enough fidelity to make the BFF's double-invocation
// question answerable for real instead of asserted away.

import http from 'node:http';
import type { AddressInfo } from 'node:net';

interface TokenRecord {
  familyId: string;
  consumed: boolean;
}

export interface RefreshRequestLog {
  refreshToken: string;
  outcome: 'consumed' | 'reuse' | 'unknown';
  issuedRefreshToken?: string;
}

export interface StubAuthApi {
  server: http.Server;
  port: number;
  /** Every `/token/refresh` call observed, in order - for assertions. */
  requests: RefreshRequestLog[];
  /** Seed a valid, unconsumed refresh token into a (new, by default) family. */
  seedRefreshToken(token: string, familyId?: string): void;
  /** Whether the given token is currently valid (present, unconsumed, family not compromised). */
  isValid(token: string): boolean;
  /** Whether the family a token belongs to has been compromised (reuse detected). */
  isFamilyCompromised(token: string): boolean;
  close(): Promise<void>;
}

let counter = 0;
function freshToken(prefix: string): string {
  counter += 1;
  return `${prefix}-${Date.now()}-${counter}`;
}

/**
 * A syntactically-valid (but unsigned) JWT with a real `exp` claim,
 * `expiresInSeconds` from now. The real backend always issues genuine JWTs;
 * `attemptRefresh()`'s `accessFresh()` (lib/auth/middleware-refresh.ts) calls
 * `decodeExpUnsafe()` (jose's `decodeJwt`, unverified) on whatever access
 * token it's handed, and a token that fails to even PARSE as a JWT makes
 * `decodeExpUnsafe` return `null`, which `accessFresh()` treats as "not
 * fresh" - i.e. a bare opaque string here (not real-token-shaped) would look
 * permanently expired and force a refresh on every single call, which is a
 * stub-fidelity bug, not a real proxy.ts behavior. Signature is a placeholder
 * (never verified by the unsafe decode path this stub exercises).
 */
function freshAccessJwt(expiresInSeconds: number): string {
  const b64url = (obj: unknown) =>
    Buffer.from(JSON.stringify(obj)).toString('base64').replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
  const exp = Math.floor(Date.now() / 1000) + expiresInSeconds;
  return `${b64url({ alg: 'none', typ: 'JWT' })}.${b64url({ exp, sub: 'stub-user' })}.stub-sig`;
}

export async function startStubAuthApi(): Promise<StubAuthApi> {
  const tokens = new Map<string, TokenRecord>();
  const compromisedFamilies = new Set<string>();
  const requests: RefreshRequestLog[] = [];

  function isFamilyCompromised(token: string): boolean {
    const rec = tokens.get(token);
    return rec ? compromisedFamilies.has(rec.familyId) : false;
  }

  function isValid(token: string): boolean {
    const rec = tokens.get(token);
    if (!rec) return false;
    return !rec.consumed && !compromisedFamilies.has(rec.familyId);
  }

  const server = http.createServer((req, res) => {
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      if (req.method === 'POST' && req.url === '/token/refresh') {
        let refreshToken: string | undefined;
        try {
          refreshToken = JSON.parse(body).refresh_token;
        } catch {
          // fall through to the unknown-token branch below
        }

        const rec = refreshToken ? tokens.get(refreshToken) : undefined;
        if (!refreshToken || !rec) {
          requests.push({ refreshToken: refreshToken ?? '(missing)', outcome: 'unknown' });
          res.writeHead(401, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ error: 'refresh token invalid' }));
          return;
        }

        const familyCompromised = compromisedFamilies.has(rec.familyId);
        if (rec.consumed || familyCompromised) {
          // Reuse/theft: mirrors PostgresRefreshStore::compromise_family -
          // revoke (consume) EVERY token in the family, including any
          // already-rotated successor, then reject this presentation.
          compromisedFamilies.add(rec.familyId);
          for (const [, r] of tokens) {
            if (r.familyId === rec.familyId) r.consumed = true;
          }
          requests.push({ refreshToken, outcome: 'reuse' });
          res.writeHead(401, { 'Content-Type': 'application/json' });
          res.end(JSON.stringify({ error: 'refresh token invalid' }));
          return;
        }

        // Win: consume this token, mint a successor in the same family. The
        // access token is a real (if unsigned) JWT with a 900s exp - see
        // freshAccessJwt() - so attemptRefresh()'s unverified exp-decode
        // treats it as fresh, exactly like a genuine backend-issued token.
        // The refresh token stays a bare opaque string, matching the real
        // TokenResponse shape ("Opaque, rotating refresh token").
        rec.consumed = true;
        const newAccess = freshAccessJwt(900);
        const newRefresh = freshToken('refresh');
        tokens.set(newRefresh, { familyId: rec.familyId, consumed: false });
        requests.push({ refreshToken, outcome: 'consumed', issuedRefreshToken: newRefresh });
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(
          JSON.stringify({
            access_token: newAccess,
            refresh_token: newRefresh,
            expires_in: 900,
            token_type: 'Bearer',
          }),
        );
        return;
      }
      res.writeHead(404, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ error: 'not_found' }));
    });
  });

  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;

  return {
    server,
    port,
    requests,
    seedRefreshToken(token: string, familyId = freshToken('family')) {
      tokens.set(token, { familyId, consumed: false });
    },
    isValid,
    isFamilyCompromised,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}
