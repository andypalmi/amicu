# Deploying to Cloudflare Workers

The amicu web app is a Next.js App Router BFF (browser-only-facing; it never
lets the browser call the Rust API directly - see `lib/auth/server-env.ts`
and RFC 0001). It deploys to Cloudflare Workers via
[`@opennextjs/cloudflare`](https://opennext.js.org/cloudflare), which
converts the standard Next.js build into a Worker.

This document covers **build/preview only**. Nobody has run `cf:deploy` from
this repo - there is no Cloudflare account wired up yet. Treat the commands
below as the local verification loop; an actual deploy is a separate,
deliberate step once an account/zone exists.

## Commands

Run from `web/` (turbo-filtered) or `web/apps/web` directly:

```bash
# Build the Worker bundle (runs `next build` under the hood, then OpenNext's
# Cloudflare transform). Fully offline - no Cloudflare account needed.
pnpm --filter web cf:build

# Build, then boot the Worker locally under workerd (Cloudflare's actual
# runtime, via wrangler) for manual verification.
pnpm --filter web cf:preview

# Build and deploy. DO NOT RUN until a Cloudflare account/zone is
# provisioned and the vars/secrets below are set for real in that account.
pnpm --filter web cf:deploy

# Regenerate `cloudflare-env.d.ts` (typed `CloudflareEnv`) after changing
# bindings in wrangler.jsonc. Gitignored - regenerate, don't hand-edit.
pnpm --filter web cf:typegen
```

`cf:preview` runs the actual Worker (not `next dev`), so it exercises the
real Workers runtime (workerd), the `nodejs_compat` compatibility layer, and
the `wrangler.jsonc` bindings/vars - the closest thing to production
available without an account.

## Required vars / secrets

`wrangler.jsonc` commits **placeholder, non-secret** values under `vars` so
the file is valid and `cf:build`/`cf:preview` work out of the box. **None of
the committed values are real.** Before any real deploy, every one of these
must be overridden in the Cloudflare dashboard ("Variables and Secrets" on
the Worker) or via `wrangler secret put <NAME>` / an environment-specific
`vars` override - **never commit real values to this file.**

| Variable | Purpose | Notes |
| --- | --- | --- |
| `API_URL` | Base URL of the Rust Management API (Cloud Run origin) | Server-only. Required in production (`server-env.ts` throws at request time if missing). Must be the Cloud Run service URL, e.g. `https://amicu-api-xxxxx-uc.a.run.app`. |
| `AUTH_ISSUER` | Expected JWT `iss` claim | **MUST equal the API's `AMICU_ISSUER`.** See "Issuer/audience parity" below. |
| `AUTH_AUDIENCE` | Expected JWT `aud` claim | **MUST equal the API's `AMICU_AUDIENCE`.** Defaults to `amicu` on both sides if unset, but don't rely on that across two independently-configured services - set both explicitly. |
| `AUTH_JWKS_URL` | JWKS endpoint used to verify access tokens offline (`jose` `createRemoteJWKSet`, see `packages/auth/src/jwt.ts`) | Defaults to `${API_URL}/.well-known/jwks.json` if unset; set explicitly if the API serves JWKS from a different host/path. |
| `NEXT_PUBLIC_SITE_URL` | Public site origin, embedded in client bundles | **Build-time**, not just runtime: `NEXT_PUBLIC_*` vars are inlined into the JS bundle during `next build`/`cf:build`, so this must be set correctly *before* building, not only in the deployed Worker's runtime vars. |
| `NODE_ENV` | Standard Node env flag | Keep `production` for any real deploy. Also gates `COOKIE_SECURE`'s default (see below) and the CSP's dev-only `'unsafe-eval'` allowance in `next.config.mjs`. |
| `COOKIE_SECURE` (optional) | Explicit override for the session cookies' `Secure` flag | Not in `wrangler.jsonc` by default - it defaults to `true` whenever `NODE_ENV !== 'development'`, which is correct for every real deployment (Workers always serves over TLS). Only set this if you need to force a non-default value; see `lib/auth/server-env.ts`. |

None of these are secrets in the credential sense (no API keys/passwords),
but `API_URL`/`AUTH_ISSUER`/`AUTH_JWKS_URL` are still environment-specific
infrastructure details that shouldn't be committed as real values - keep
using Cloudflare's vars/secrets store per environment (staging vs prod).

### Issuer/audience parity (hard rule)

The web BFF verifies access tokens offline against the API's JWKS using
`AUTH_ISSUER`/`AUTH_AUDIENCE` (`packages/auth/src/jwt.ts`, `jose`
`createRemoteJWKSet`). If these don't **exactly** match the Rust API's own
`AMICU_ISSUER`/`AMICU_AUDIENCE` (whatever the API is configured to stamp
into `iss`/`aud` when it signs tokens), every request fails JWT
verification and every user is effectively logged out. Set these as a pair,
always, whenever either service's issuer/audience config changes.

## Cloudflare-specific implementation notes

- **`nodejs_compat` + `global_fetch_strictly_public`**: required by
  `@opennextjs/cloudflare` for Next.js's Node-API-shaped server code, and
  recommended by the OpenNext Cloudflare guide, respectively.
- **`allow_importable_env`**: `lib/auth/config.ts` reads `process.env` at
  *module evaluation time* (not inside a request handler) so the fail-fast
  config check (`server-env.ts`) runs before the first auth-touching route
  executes. Cloudflare Workers only populate `process.env` from
  bindings/vars once a request context exists by default; this
  compatibility flag makes vars importable at module-eval time too, which is
  what OpenNext's docs recommend for this exact pattern instead of
  restructuring the config module to lazily read `process.env` per-request.
- **`COOKIE_SECURE`**: previously inferred as `NODE_ENV === 'production'`.
  Now an explicit `server-env.ts` field (`COOKIE_SECURE`, default `true`
  unless `NODE_ENV === 'development'`) so the `Secure` cookie attribute
  doesn't silently depend on exactly how a given runtime sets `NODE_ENV`.
  Local `next dev` behavior is unchanged (still non-Secure, since `next dev`
  sets `NODE_ENV=development`).
- **Assets binding**: `wrangler.jsonc`'s `assets.directory` points at
  OpenNext's `.open-next/assets` output (Next.js's static/public assets).
  This app does not use `next/image`, so no `images` binding is configured.
- No KV/R2/D1/Durable Object bindings are configured - the app has no
  ISR/on-demand-revalidation or persistent-storage requirements today (see
  `open-next.config.ts`).

## Local verification (already run before this PR)

```bash
pnpm --filter web cf:build     # offline, must succeed
pnpm --filter web cf:preview   # boots the Worker under workerd locally
# then, in another shell:
curl -i http://127.0.0.1:<port>/
curl -i http://127.0.0.1:<port>/api/<some-bff-route>
# Ctrl-C the preview process when done.
```

With `API_URL` pointing at the placeholder host above, BFF routes that call
the API are expected to fail (DNS/connect error surfaced as a 4xx/5xx) - the
point of this check is that the Worker itself boots, resolves the App
Router, and routes matching requests, not that the placeholder backend
responds successfully.
