# Contributing to the web app

The frontend lives under `web/`: a Turborepo + pnpm workspace with the Next.js app (`apps/web`), the shared design system, and config packages. See [`README.md`](./README.md) for the layout.

## Prerequisites

- Node.js >= 20.9.0
- pnpm (`corepack enable pnpm`)

## Install and run

```bash
cd web
pnpm install
pnpm dev        # apps/web: marketing, app shell, and /docs
```

Routes: `/` (marketing), `/app` (product shell, placeholder), `/docs` (documentation).

## Checks

Run from `web/` before opening a pull request:

```bash
pnpm install --frozen-lockfile
pnpm check:generated
pnpm lint
pnpm lint:links
pnpm typecheck
pnpm test
cd apps/web && NEXT_PUBLIC_SITE_URL=https://amicu.dev pnpm cf:build
```

## Link check

`pnpm lint:links` validates every internal link (docs MDX, `#heading` anchors, and `href`s in `app/`, `components/`) against the real route set (`next-validate-link` + Fumadocs source); external links are skipped. It runs in CI and as a git pre-commit hook (`.githooks/pre-commit`, enabled by `pnpm install` through the `prepare` script, or manually with `git config core.hooksPath .githooks`). The hook only runs when `web/` files are staged and is skipped if pnpm is not installed; bypass with `git commit --no-verify`. Hooks live in `.githooks` and chain to any local `.git/hooks` hook of the same name. The checker validates physical routes only and ignores the `amicu.dev` / `app.amicu.dev` host split.

## Commit messages

Do not put Claude / Claude Code attribution (session links, `Co-Authored-By: Claude`, "Generated with ...") in commit messages: the `commit-msg` hook strips such lines and fails the commit (re-run with `git commit -F .git/COMMIT_EDITMSG`), and CI checks every PR commit. Any line mentioning Claude is stripped on purpose, not only attribution trailers. Test the check with `sh .githooks/test-no-claude-attribution.sh`.

## Writing docs

Public documentation is MDX under `apps/web/content/docs`, rendered by Fumadocs. Each folder has a `meta.json` that sets the page order; a page missing from its `meta.json` still builds but does not appear in the sidebar.

- Describe the CLI as it behaves today. Port field names, flags, and codes from the code (`crates/amicu-hcl`, `crates/amicu-core`, `crates/amicu-cli`) and from `crates/amicu-cli/skills/`. Do not invent any.
- Validate every complete HCL example with `amicu validate` before committing it. Use fenced blocks tagged `hcl` only for examples that parse; tag illustrative fragments `text`.
- Never put secret values in examples. Use `secret("NAME")` references.
- Keep the tone short and technical. No marketing copy.
- Contributor notes belong here, not in `/docs`.

## AI-friendly exports

Fumadocs generates machine-readable exports at build time:

- `/llms.txt`: index of documentation pages
- `/llms-full.txt`: full documentation as one text stream
- `/llms.mdx/docs/<page>/content.md`: per-page Markdown

## Notes

- `packages/api-client` and `packages/auth` are intentional stubs. See their READMEs.
- The docs live in a single app at `/docs`; a `docs.amicu.dev` split is deferred.
- A local `next start` served over plain http needs `COOKIE_SECURE=false`, otherwise Secure session cookies are dropped.
