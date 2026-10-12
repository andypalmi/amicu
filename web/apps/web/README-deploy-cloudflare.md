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

# Build and deploy (passes --keep-vars to `wrangler deploy` so it does NOT
# overwrite dashboard-set vars with the committed placeholders - see the
# warning below). DO NOT RUN until a Cloudflare account/zone is provisioned
# and the vars/secrets below are set for real in that account.
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
| `API_URL` | Base URL of the Rust Management API (Cloud Run origin) | Server-only. Required in production (`server-env.ts` throws at request time if missing). Production: `https://api.amicu.dev` (the API's custom domain in front of Cloud Run). |
| `AUTH_ISSUER` | Expected JWT `iss` claim | Production: `https://api.amicu.dev`. **MUST equal the API's `AMICU_ISSUER`.** See "Issuer/audience parity" below. |
| `AUTH_AUDIENCE` | Expected JWT `aud` claim | **MUST equal the API's `AMICU_AUDIENCE`.** Defaults to `amicu` on both sides if unset, but don't rely on that across two independently-configured services - set both explicitly. |
| `AUTH_JWKS_URL` | JWKS endpoint used to verify access tokens offline (`jose` `createRemoteJWKSet`, see `packages/auth/src/jwt.ts`) | Defaults to `${API_URL}/.well-known/jwks.json` if unset; set explicitly if the API serves JWKS from a different host/path. |
| `NEXT_PUBLIC_SITE_URL` | Public site origin, embedded in client bundles. Production: `https://amicu.dev` (also the source of the expected Turnstile hostname) | **Build-time**, not just runtime: `NEXT_PUBLIC_*` vars are inlined into the JS bundle during `next build`/`cf:build`, so this must be set correctly *before* building, not only in the deployed Worker's runtime vars. |
| `NODE_ENV` | Standard Node env flag | Keep `production` for any real deploy. Also gates `COOKIE_SECURE`'s default (see below) and the CSP's dev-only `'unsafe-eval'` allowance in `next.config.mjs`. |
| `COOKIE_SECURE` (optional) | Explicit override for the session cookies' `Secure` flag | Not in `wrangler.jsonc` by default - it defaults to `true` whenever `NODE_ENV !== 'development'`, which is correct for every real deployment (Workers always serves over TLS). Only set this if you need to force a non-default value; see `lib/auth/server-env.ts`. |
| `NEXT_PUBLIC_TURNSTILE_SITE_KEY` (optional) | Cloudflare Turnstile public site key | **Build-time** (inlined). Unset => widget not rendered. Create the widget in the Cloudflare dashboard (hostname = the public site host). |
| `TURNSTILE_SECRET_KEY` (optional) | Turnstile secret for server-side siteverify | **Secret** - `wrangler secret put TURNSTILE_SECRET_KEY`; never `NEXT_PUBLIC`, never in `vars`. Unset => verification skipped (dev). Set BOTH keys together. Verification is **fail-closed**: if the secret is set and the token is missing or Cloudflare is unreachable/times out (5s), signup/forgot/resend show a generic error. Checks `success`, `action` and hostname (from `NEXT_PUBLIC_SITE_URL`); forwards `CF-Connecting-IP`. |

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
  executes. That keeps working under OpenNext, but **not because of this
  flag** - `allow_importable_env` is just the explicit spelling of the
  Workers default (it only changes behavior paired with its opposite,
  `disallow_importable_env`, which we don't set, so it's currently a no-op
  here). The actual mechanism is `@opennextjs/cloudflare` calling its own
  `populateProcessEnv()` on every request, which copies the Worker's `env`
  bindings onto `process.env` before Next.js server code runs - including
  code that only executes once per isolate (module-eval-time reads like
  ours). That per-request population is itself gated by the
  `nodejs_compat_populate_process_env` compatibility flag, which defaults to
  ON for any `compatibility_date` after `2025-04-01` (ours is `2026-09-16`).
  We keep `allow_importable_env` in `wrangler.jsonc` anyway for
  explicitness, not because it's load-bearing.
- **`COOKIE_SECURE`**: previously inferred as `NODE_ENV === 'production'`.
  Now an explicit `server-env.ts` field (`COOKIE_SECURE`, default `true`
  unless `NODE_ENV === 'development'`) so the `Secure` cookie attribute
  doesn't silently depend on exactly how a given runtime sets `NODE_ENV`.
  Local `next dev` behavior is unchanged (still non-Secure, since `next dev`
  sets `NODE_ENV=development`).
- **Custom domain**: `wrangler.jsonc` has `routes: [{ pattern: "amicu.dev",
  custom_domain: true }]`, so `cf:deploy` attaches the Worker to the apex of
  the `amicu.dev` zone (DNS record + cert managed by Cloudflare). `www.amicu.dev`
  redirects to the apex (infra). The API is on `https://api.amicu.dev`.
  `workers_dev` is explicitly `false` (no `*.workers.dev` URL), and
  `dev.host: "localhost"` stops `wrangler dev`/`cf:preview` from rewriting
  request URL/Host/Origin to `amicu.dev`.
- **Before approving the first production deploy**: set the repo variable
  `NEXT_PUBLIC_SITE_URL=https://amicu.dev` (build-time) and add `amicu.dev`
  to the Turnstile widget's hostnames.
- **Assets binding**: `wrangler.jsonc`'s `assets.directory` points at
  OpenNext's `.open-next/assets` output (Next.js's static/public assets).
  This app does not use `next/image`, so no `images` binding is configured.
- No KV/R2/D1/Durable Object bindings are configured - the app has no
  ISR/on-demand-revalidation or persistent-storage requirements today (see
  `open-next.config.ts`).
- **Node.js middleware**: `proxy.ts` (the app's middleware, silent-refresh +
  docs rewrites) runs as **Node.js middleware** under this adapter -
  `opennextjs-cloudflare build` prints `WARN Node.js middleware support is
  experimental in cloudflare, and not officially maintained by OpenNext
  maintainers`. This is not a configuration choice: Next.js 16 does not
  offer an edge-runtime option for `proxy.ts` at all (there is no `export
  const runtime = 'edge'` to opt into for this file), so it always runs as
  Node.js middleware, adapter or not. Both `cf:build` and `cf:preview`
  worked correctly against this in testing, but treat it as an
  upstream-tracked caveat rather than something this repo can "fix" by
  choosing a different runtime.

> [!WARNING]
> **`wrangler deploy` (bare) DELETES every plain dashboard-set `vars` and
> replaces them with whatever is committed in `wrangler.jsonc`'s `vars`
> block** - i.e. the placeholder values above. This is standard Wrangler
> behavior (each deploy's `vars` fully replaces the previous deploy's
> `vars`), not something specific to this adapter, but it's an easy way to
> accidentally overwrite a real `API_URL`/`AUTH_ISSUER`/etc. with
> `https://REPLACE-ME.example.invalid` and take the whole app down.
>
> Two independent mitigations, both already in place:
> - **`cf:deploy` passes `--keep-vars`** (`opennextjs-cloudflare deploy --
>   --keep-vars`, which forwards straight through to `wrangler deploy
>   --keep-vars`), which preserves whatever plain vars are already set on the
>   Worker instead of overwriting them with the committed placeholders.
> - **Prefer `wrangler secret put <NAME>` over dashboard "vars" for
>   anything environment-specific** (`API_URL`, `AUTH_ISSUER`,
>   `AUTH_AUDIENCE`, `AUTH_JWKS_URL`, `NEXT_PUBLIC_SITE_URL`\*). Secrets are
>   **never** touched by `wrangler deploy`, `--keep-vars` or not - they're
>   the safer default for anything that would be bad to accidentally reset.
>   (\* `NEXT_PUBLIC_SITE_URL` is the one exception: since it must be present
>   at *build* time, not just runtime, it can't be a Workers secret - it has
>   to be set in the environment that runs `cf:build`/`cf:deploy`, e.g. CI
>   secrets, not the Worker's own secret store.)
>
> If you ever do need to intentionally reset `vars` to the committed
> placeholders (e.g. tearing down an environment), run bare `wrangler
> deploy` deliberately - never as a reflex.

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
