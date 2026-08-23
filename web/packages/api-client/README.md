# @amicu/api-client

Typed TypeScript client for the Amicu **Management API**, **generated only** from
that API's committed OpenAPI spec. It is never hand-written or hand-edited, and CI
enforces zero drift.

## Source of truth

`crates/amicu-schema/openapi.json` (published by the Rust API via `utoipa`). It
documents the auth routes (`/signup`, `/token`, `/token/refresh`, `/logout`,
`/recover`, `/reset`, `.well-known/*`) and the auth-gated engine routes
(`/validate`, `/plan`).

## What's generated vs. hand-written

| File | Status | Notes |
| --- | --- | --- |
| `src/schema.ts` | **generated** | `paths` / `components` / `operations` + root type aliases. Do not edit. Carries the `openapi-typescript` "auto-generated" header. |
| `src/client.ts` | hand-written | Thin `openapi-fetch` wrapper: base-URL config only, **no business logic**. |
| `src/index.ts` | hand-written | Public re-exports. |

## Tooling (pinned)

- [`openapi-typescript`](https://openapi-ts.dev) `7.13.0` - spec → `src/schema.ts`.
- [`openapi-fetch`](https://openapi-ts.dev/openapi-fetch/) `0.17.0` - the runtime
  client (`createApiClient`), fully typed from `paths`.

## Regenerating after an API change

When the Rust API changes the spec, rerun:

```sh
pnpm --filter @amicu/api-client generate
```

Then commit the updated `src/schema.ts`. That's it - the committed file is a pure
function of the spec.

## CI drift check

```sh
pnpm --filter @amicu/api-client check:generated
```

Regenerates `src/schema.ts` and fails (`git diff --exit-code`) if it no longer
matches what's committed. Wired into the web CI job (`.github/workflows/ci.yml`)
and exposed as the `check:generated` turbo task, so a spec change that isn't
regenerated turns CI red.

## Usage (server-only)

```ts
import { createApiClient } from '@amicu/api-client';

const api = createApiClient({ baseUrl: process.env.API_URL! });
const { data, error, response } = await api.POST('/token', {
  body: { email, password },
});
```

The base URL is a **server-only** value - never a `NEXT_PUBLIC_*` env, and these
calls must never run in the browser. In `apps/web`, the BFF's `@amicu/auth/server`
auth client is built on top of this package.
