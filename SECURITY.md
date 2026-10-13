# Security policy

## Supported versions

amicu is pre-1.0. Security fixes go to the latest release and to `main`. Older versions are not patched.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub private vulnerability reporting: open the repository's **Security** tab and choose **Report a vulnerability**, or go directly to <https://github.com/andypalmi/amicu/security/advisories/new>.

Please do not open a public issue, pull request or discussion for a vulnerability.

Include, as far as you can:

- The affected component (CLI, API/server, or web) and version or commit.
- A description of the issue and its impact.
- Steps or a minimal proof of concept to reproduce it.
- Any suggested fix or mitigation.

## What to expect

We aim to acknowledge a report within a few business days, then keep you updated as we investigate and fix it. This is a small project, so we do not offer fixed response or fix timelines. We will credit you in the advisory unless you prefer otherwise.

## Scope

In scope:

- The `amicu` CLI and the crates in this repository (secret handling, endpoint authentication checks, state, network access).
- The hosted app and API (`amicu-server`, `amicu-api`): authentication, sessions, workspace isolation.
- The web app under `web/` (marketing site, docs, console).

Out of scope: vulnerabilities in third-party providers or services (for example Modal or Hugging Face), which should be reported to them; and issues that need an already-compromised machine or account.

For how the CLI treats secrets, endpoint keys, state and telemetry, see the [security docs](https://amicu.dev/docs/security) and the [security page](https://amicu.dev/security).
