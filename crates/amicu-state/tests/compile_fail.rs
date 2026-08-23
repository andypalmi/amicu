//! Compile-fail guard: a persisted-state type must not be able to hold a
//! [`SecretValue`](amicu_core::secret::SecretValue). State is written to
//! canonical JSON for checksumming and persistence (see
//! `StateSnapshot`/`checksum_of` in `src/snapshot.rs`), and `#[derive(Serialize)]`
//! on a state-shaped struct requires every field to implement `Serialize`.
//! Because `SecretValue` deliberately does not implement `Serialize`, embedding
//! one anywhere in a serialized state type is a compile error, not a runtime
//! policy check. Each case under `tests/compile_fail/` must fail to compile.

// stderr is pinned to rustc 1.97 diagnostics, so only run under that toolchain.
#[test]
#[rustversion::attr(
    not(stable(1.97)),
    ignore = "trybuild stderr is pinned to rustc 1.97 diagnostics"
)]
fn secret_value_is_unrepresentable_in_persisted_state() {
    trybuild::TestCases::new().compile_fail("tests/compile_fail/*.rs");
}
