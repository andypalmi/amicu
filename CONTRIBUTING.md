# Contributing to amicu

Thanks for helping. This repo is a Rust workspace (`crates/`) plus a pnpm/Turborepo web monorepo (`web/`). For web-specific details see [`web/CONTRIBUTING.md`](./web/CONTRIBUTING.md).

Report security issues privately, not as public issues: see [`SECURITY.md`](./SECURITY.md).

## Prerequisites

- Rust 1.97 or newer (`rust-version` in `Cargo.toml`; CI also builds on the latest stable). Components: `rustfmt`, `clippy`.
- Node.js >= 20.9.0 (CI uses 22) and pnpm 11.22.0 (`packageManager` in `web/package.json`; `corepack enable pnpm`). Only needed for `web/`.
- Docker, only if you run the Postgres-backed tests (see below).
- Optional tools: `cargo install cargo-audit`, `cargo install sqlx-cli` (provides `cargo sqlx`).

## Build the CLI

```bash
cargo build --release -p amicu-cli   # binary: ./target/release/amicu
```

## Tests that need Postgres

`cargo test --workspace` runs without a database. Some `#[ignore]`d tests need a live Postgres through `DATABASE_URL`. For the HTTP tenancy tests:

```bash
docker compose up -d postgres
export DATABASE_URL=postgres://amicu_app:amicu_app@localhost:5432/amicu
cargo test -p amicu-api --test tenancy_http -- --ignored
```

Use the `amicu_app` role, not the bootstrap superuser: row-level security is not enforced for superusers. See "Development database" in the [README](./README.md).

Do not run `cargo test --workspace -- --ignored`: it also runs other ignored tests with their own requirements.

- The `compile_fail` guards in `amicu-core` and `amicu-state` are pinned to rustc 1.97; run them with `cargo +1.97` as in the CI msrv job (see below).
- The other live Postgres tests need their own setup (custom roles and databases), described in their module docs: `crates/amicu-server/tests/rls_guard_live.rs` and `crates/amicu-server/tests/rate_limit_postgres_live.rs`.

## Run the CI checks locally

Rust (from the repo root; CI uses the stable toolchain):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo build --workspace --all-targets --locked
cargo test --workspace --locked
cargo audit
# MSRV build and the pinned compile-fail guard:
cargo +1.97 build --workspace --all-targets --locked
cargo +1.97 test -p amicu-core -p amicu-state --test compile_fail -- --include-ignored
```

CI also builds the `Dockerfile` (no push), which compiles against the committed `.sqlx/` cache. To check that cache locally against a running database:

```bash
cargo sqlx prepare --workspace --check -- --all-targets
```

Web (from `web/`, after `pnpm install --frozen-lockfile`):

```bash
pnpm check:generated
pnpm check:colors
pnpm lint
pnpm typecheck
pnpm lint:links
pnpm test
pnpm build
```

CI additionally runs `pnpm audit --audit-level=high`. Dependency audits only block a PR that touches the relevant lockfile or manifest.

## Git hooks

`pnpm install` in `web/` sets `core.hooksPath=.githooks` (or run `git config core.hooksPath .githooks`).

- `pre-commit`: when `web/` files are staged, checks token-only colours (`check:colors`) and internal links (`pnpm lint:links`); skipped if node/pnpm or `web/node_modules` is missing.
- `commit-msg`: rejects commit messages containing Claude / Claude Code attribution. CI re-checks every PR commit.

## Generated artifacts

Commit these when their source changes; CI fails on drift.

- `.sqlx/` query cache, after changing any `sqlx::query!` or migration: `cargo sqlx prepare --workspace -- --all-targets` (needs `DATABASE_URL`).
- `web/packages/api-client/src/schema.ts`, after changing `crates/amicu-schema/openapi.json`: `pnpm --filter @amicu/api-client generate` (from `web/`).
- `web/apps/web/lib/generated/tokens.ts`, after editing `web/packages/ui/src/styles/tokens.css`: `pnpm gen:tokens` (from `web/`; `pnpm check:generated` verifies it).

## Commits and pull requests

- Commit subjects look like `area: summary`, for example `web: rebuild providers page as a plain registry list` or `docs: accurate install instructions`. Keep them short and imperative. `fix(area): ...` / `feat(area): ...` also appear in history.
- Open PRs against `main`, reference the issue they address (for example `Closes #123`), and keep each PR to one concern. Dependency bumps go in their own PR.
- CI must be green before merge.
- PR titles are checked by CI and, because PRs are squash-merged with the PR title as the commit message, must be Conventional Commits: `type(scope)!: summary`. Types: feat, fix, docs, chore, refactor, perf, test, ci, build. Scopes: cli, engine, providers, state, modal, web, docs, server. `feat` releases a minor, `fix`/`perf` a patch. See [`RELEASING.md`](./RELEASING.md).

## License

amicu is licensed under AGPL-3.0 (see [`LICENSE`](./LICENSE)). By submitting a contribution you agree that it is licensed under the same terms. There is no CLA or DCO sign-off.
