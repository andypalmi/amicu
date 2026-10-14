# Signing-key rotation runbook

Operational procedure for the durable JWT signing key.
Covers generating a key, first deploy, rotating
a key across replicas, and how long a retired key must stay published before
it is safe to drop.

See also: `crates/amicu-server/src/signing_key.rs` (the loader implementation
and its unit tests) and `amicu-server`'s crate-level docs (the "Multi-replica
signing key" section).

## Background

`amicu-server` mints access tokens as Ed25519-signed JWTs and publishes the
public half of every active/retiring key at `GET /.well-known/jwks.json`. Each
key's `kid` is deterministically derived — `SHA256(public_key)` truncated to
22 base64url characters (132 bits, widened from an original 16/96 bits)
— so the SAME key always produces the SAME
`kid` on every replica, with no coordination or handshake required between
them.

Two environment variables (each with a `_FILE` alternative) control the
keyring:

| Variable | Purpose |
|---|---|
| `AMICU_SIGNING_KEY_PEM` / `AMICU_SIGNING_KEY_PEM_FILE` | The ACTIVE key: signs new tokens, and is published in JWKS. |
| `AMICU_SIGNING_KEYS_RETIRING_PEM` / `AMICU_SIGNING_KEYS_RETIRING_PEM_FILE` | Zero or more RETIRING keys (concatenated PEM blocks): verify-only, never sign, kept published in JWKS so tokens they already signed keep verifying until they expire. |

Setting both the value and the `_FILE` variant of the same logical key (active
or retiring) is a hard startup error — fail loud, not silently pick one.

Every replica of a deployment MUST be given identical values for these
variables (e.g. by pointing every replica at the same secret-manager secret
version). Because `kid` is a pure function of the public key, identical env ⇒
identical keyring ⇒ identical JWKS on every replica, with zero runtime
coordination.

## Generating a key

Preferred: the `amicu-server gen-signing-key` subcommand prints a fresh
Ed25519 PKCS#8 PEM to stdout and nothing else, so it's safe to pipe straight
into a secret manager:

```sh
amicu-server gen-signing-key | gcloud secrets versions add \
    AMICU_SIGNING_KEY_PEM --data-file=-
```

Equivalently, with `openssl` (useful outside a built `amicu-server` binary):

```sh
openssl genpkey -algorithm ed25519 -out key.pem
```

Both produce an unencrypted PKCS#8 PEM — exactly the format
`SigningKey::from_pkcs8_pem` expects. No further conversion is needed; the
raw PEM text can be used verbatim as `AMICU_SIGNING_KEY_PEM` (or written to a
file and pointed to via `AMICU_SIGNING_KEY_PEM_FILE`).

If your secret manager or shell cannot preserve real newlines in an
environment variable, a literal `\n`-escaped (or `\r\n`-escaped) single-line
value is also accepted: the loader normalises literal escape sequences and
real CRLF/LF alike back to real newlines before parsing.

## First deploy

1. Generate one key as above.
2. Set `AMICU_SIGNING_KEY_PEM` (or mount the file and set
   `AMICU_SIGNING_KEY_PEM_FILE`) identically on every replica — in practice,
   via the infra secret manager, so every replica reads the same secret
   version rather than a value copy-pasted per replica.
3. Set `AMICU_ENV=production`. In production, a missing active key is a hard
   startup error (`Error::MissingSigningKey`) rather than a silent per-boot
   ephemeral fallback — this is deliberate: an ephemeral key differs across
   replicas and does not survive a restart, so production must never start
   without a real one.

## Rotation procedure

1. **Generate a NEW key** the same way as above (`amicu-server
   gen-signing-key`, or `openssl genpkey -algorithm ed25519 -out
   key-new.pem`).
2. **Move the OLD active key to retiring, promote the new key to active**:
   - Set `AMICU_SIGNING_KEYS_RETIRING_PEM` to the OLD active key's PEM
     (concatenate with any already-retiring keys' PEM blocks if there are
     more than one — the loader splits a concatenated blob on `-----BEGIN`).
     Do not simply discard the old key; tokens it already signed are still
     live for up to its access-token TTL.
   - Set `AMICU_SIGNING_KEY_PEM` to the NEW key's PEM.
3. **Deploy the identical env to every replica.** Because `kid` is
   deterministically derived from the public key, every replica converges to
   the exact same keyring and the exact same JWKS with zero coordination —
   there is no handshake, no leader election, and no ordering requirement
   between replicas picking up the new env.
4. **After the retention window (below) has passed**, drop the old key from
   `AMICU_SIGNING_KEYS_RETIRING_PEM` (or clear the variable entirely if there
   are no other retiring keys) and redeploy.

## Retention window: how long to keep a retiring key

Access tokens are the ONLY JWTs this key signs — refresh tokens are opaque,
random, server-side-stored tokens and are never JWT-signed with a
`SigningKey`. The retention window only needs to outlive access tokens and
verifier caching, not sessions:

- Access tokens live at most `access_ttl` = **5 minutes**
  (`IssuerConfig::new`'s default).
- An external verifier using `HttpJwksProvider` (a different service, in a
  different process, consuming this server's JWKS over HTTP) may cache a
  fetched JWKS for up to `cache_ttl` = **5 minutes**
  (`VerifierConfig::new`), and may serve it stale for up to `max_stale` =
  **1 hour** during a provider outage.

So the **hard minimum** safe retention is on the order of ~10-15 minutes
(one access-token TTL plus one JWKS cache cycle). In practice:

- **Recommended minimum: 1 hour** — covers `max_stale` with margin, so even a
  verifier riding out a JWKS-fetch outage on stale cached keys never loses the
  retiring key mid-outage.
- **Recommended default: 24 hours** — covers a slow or staggered rollout
  across replicas, clock skew between hosts, and gives an operator debugging
  an incident room to look at recently-issued tokens. There is no real cost
  to keeping a retiring key published longer than necessary; it is just one
  more entry in the JWKS document.

## What breaks if a retiring key is dropped too early

Any still-live access token signed by the dropped key immediately fails
verification (`AuthError::UnknownKid`, surfaced as `401 Unauthorized`) the
moment ANY verifier — this server's own in-process `KeyringJwksProvider`, or
an external `HttpJwksProvider` consumer once its cache naturally expires — no
longer has that key published.

Sessions themselves recover on their next silent token refresh: refresh
tokens are unaffected (never signed by this key), so the client transparently
re-authenticates and gets a fresh access token signed by the current active
key. But the specific affected access token is dead immediately, not
gracefully, until that refresh happens.

## Multi-replica guarantee

Identical `AMICU_SIGNING_KEY_PEM` / `AMICU_SIGNING_KEYS_RETIRING_PEM` env on
every replica of a deployment implies an identical keyring, which implies an
identical published JWKS on every replica. Because `kid` is deterministically
derived from the public key (`SHA256(pubkey)` truncated to 22 base64url
characters), this holds with **zero coordination or handshake** between
replicas — each one converges to the same key set purely by reading the same
secret, independently. A token minted by one Cloud Run replica verifies on
any other replica of the same deployment.

Separately: the engine-gate verifier now reads a LIVE, in-process keyring
(`KeyringJwksProvider`) rather than a boot-time snapshot, so IF something in
a given process calls `AuthService::rotate_keys`/`retire` at runtime (there
is no HTTP admin endpoint for this today — the documented operational
rotation path is the env-var-plus-redeploy procedure above), that replica's
own engine gate would pick it up with no restart and no network round-trip —
though not instantly: the verifier's own cache still gates how fast a change
is actually observed (worst case ~60s for a brand-new `kid` under a
concurrent unknown-`kid` flood; up to 5 minutes for a retired key's cached
decoding key to be evicted — see `KeyringJwksProvider`'s doc comment in
`crates/amicu-auth/src/verifier.rs` for the exact mechanism).

## Infra ownership

The `AMICU_SIGNING_KEY_PEM` (and `AMICU_SIGNING_KEYS_RETIRING_PEM`) secret is
expected to be managed by the infra Terraform repo
(a separate, private repo, out of scope for this
document) — that is where the secret-manager resource, its versions, and the
per-replica env wiring should live. This runbook only covers the operational
procedure once that plumbing exists.

## The `_FILE` variants

`AMICU_SIGNING_KEY_PEM_FILE` and `AMICU_SIGNING_KEYS_RETIRING_PEM_FILE` are
alternatives to the raw-value variables, for a mounted-secret-volume
deployment shape (e.g. a Kubernetes/Cloud Run secret mount) instead of an
inline env value. Setting BOTH the value and file variant for the same
logical key (active or retiring) is a startup error — the loader fails loud
rather than silently picking one source over the other.

## The development fallback is NOT for production

Absent `AMICU_SIGNING_KEY_PEM` (and its `_FILE` variant), `amicu-server`
falls back to an ephemeral `SigningKey::generate()` on every boot, with a
loud `WARNING` printed to stderr. This is intentional zero-config convenience
for local development ONLY:

- The key is regenerated every process start — it does not survive a
  restart, so every previously issued token becomes unverifiable the moment
  the process restarts.
- Each replica that falls back generates its OWN independent key — tokens
  minted by one replica fail verification on any other.

`AMICU_ENV=production` refuses to start at all without a real active signing
key configured (`Error::MissingSigningKey`), specifically so this fallback
can never silently reach a real multi-replica deployment.
