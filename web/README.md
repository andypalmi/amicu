# Amicu web

The Amicu frontend - marketing site, product app, and documentation - as a single
Next.js (App Router) application, plus its shared packages. A **thin client** over
the Rust Management API: it renders questions, candidates, plans, and metrics, but
owns no planning, recommendation, policy, or capability logic (RFC 0001 §6).

Licensed **AGPL-3.0-or-later** in full (see [`LICENSE`](./LICENSE) and RFC 0001 §9).
This scaffold delivers RFC 0001 slices **1** (monorepo + design system) and **11**
(Fumadocs `/docs` + `llms.txt`).

## Layout

```
web/                          # pnpm workspace root (Turborepo)
├── apps/
│   └── web/                  # Next.js App Router: marketing + app + docs
│       ├── app/(marketing)/  # public landing (SSG)
│       ├── app/(app)/        # authed product shell (placeholder; wires auth)
│       ├── app/docs/         # Fumadocs, single-app (multi-zone deferred)
│       ├── app/api/          # thin BFF route handlers (no domain logic)
│       ├── app/llms.txt/     # + llms-full.txt, llms.mdx - AI-agent exports
│       └── content/docs/     # MDX docs (see CONTRIBUTING.md)
├── packages/
│   ├── ui/                   # shared shadcn design system + Tailwind v4 tokens
│   ├── config/               # shared tsconfig + eslint presets
│   ├── api-client/           # STUB - generated-only client (see its README)
│   └── auth/                 # STUB - JWKS verify + session
├── turbo.json
└── pnpm-workspace.yaml
```

## Stack

Next.js 16 (App Router) · TypeScript · Tailwind CSS v4 · shadcn/ui · Fumadocs ·
Turborepo + pnpm. Theme-aware; restrained motion (dev-infra audience).

## Prerequisites

- Node.js >= 20.9.0
- pnpm - `corepack enable pnpm`

## Commands

Run from `web/`:

```bash
pnpm install
pnpm dev         # turbo dev - apps/web on http://localhost:3000
pnpm build       # turbo build (includes the /docs route + llms.txt)
pnpm lint
pnpm typecheck
```

Routes: `/` (marketing), `/app` (product shell), `/docs` (documentation),
`/llms.txt` and `/llms-full.txt` (AI-agent exports, generated at build).

## Notes

- `packages/api-client` and `packages/auth` are intentional **stubs** - see their
  READMEs. Do not implement them here.
- The docs live in a single app at `/docs`; a `docs.amicu.dev` split / Vercel
  multi-zone is deferred (RFC 0001 §7).

Note: a local `next start` or preview served over plain http needs `COOKIE_SECURE=false`, otherwise the session cookies (Secure flag) are dropped and sign-in will not stick.
