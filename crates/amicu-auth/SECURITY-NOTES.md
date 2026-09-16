# Security notes: amicu-auth

## RSA (RUSTSEC-2023-0071, Marvin Attack) — accepted risk, not exploitable here

`amicu-auth` depends on `jsonwebtoken = { version = "11", default-features = false, features = ["rust_crypto"] }`.
This app signs and verifies **EdDSA (Ed25519) only** — see `Algorithm::EdDSA` in
`src/key.rs` (`Header::new`) and `src/verifier.rs` (`Validation::new`); no code
path ever constructs an `Algorithm::RS256`/`RS384`/`RS512`/`PS*` header or
validation, and there is no RSA key material anywhere in this crate.

We investigated dropping the `rsa` crate from the dependency tree (it is
`rsa 0.9.10`, flagged by `cargo audit` for RUSTSEC-2023-0071, the Marvin
timing-sidechannel attack, which has **no upstream fix**). Conclusion: **it
cannot be excluded without changing the crypto backend**, which is out of
scope for a targeted cleanup:

- `jsonwebtoken` 11's Cargo features (from its `Cargo.toml`):
  ```toml
  [features]
  default = ["use_pem"]
  use_pem = ["dep:pem", "dep:simple_asn1"]
  rust_crypto = [
      "dep:ed25519-dalek", "dep:hmac", "dep:p256", "dep:p384",
      "dep:rand", "dep:rsa", "dep:sha2",
  ]
  aws_lc_rs = ["dep:aws-lc-rs"]
  ```
  The `rust_crypto` backend feature bundles `rsa` unconditionally alongside
  `ed25519-dalek`/`hmac`/`p256`/`p384` — there is no finer-grained sub-feature
  to opt out of just RSA while keeping EdDSA support on this backend.
- The only way to drop `rsa` from the lockfile is to switch to the
  `aws_lc_rs` backend feature instead of `rust_crypto`. That replaces the
  pure-Rust crypto stack with AWS-LC (compiled C/assembly via `aws-lc-sys`),
  which needs a `cmake` build-time toolchain (not currently installed/assumed
  in this repo's dev or CI environments) and contradicts the deliberate
  "pure Rust, hermetic, holds MSRV 1.97" backend choice already documented
  next to this dependency in `Cargo.toml`. Swapping crypto backends is an
  architecture decision, not a scoped dependency cleanup, so we did not do it
  here.

**Accepted risk**: `rsa` stays in the dependency tree as dead code (compiled
in via `jsonwebtoken`'s `rust_crypto` feature bundle but never invoked — no
`Algorithm::RS*`/`PS*` value is ever constructed by `amicu-auth` or its
callers). The Marvin attack requires the victim to actually perform RSA
PKCS#1v1.5 decryption/signing operations with attacker-observable timing;
since this codebase never does that, the advisory does not describe an
exploitable condition here.

**Removal condition**: re-evaluate when either (a) `jsonwebtoken` ships a
`rust_crypto`-compatible feature split that excludes `rsa`, or (b) the team
decides to take on the `aws_lc_rs` backend (and its `cmake` build
dependency) project-wide, or (c) a maintained non-RSA-coupled JWT crate
replaces `jsonwebtoken`. Tracked via the `cargo audit` ignore in
`.cargo/audit.toml`.
