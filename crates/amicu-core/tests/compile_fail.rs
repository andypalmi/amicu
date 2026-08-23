//! Compile-fail guard: `SecretValue` must not gain traits that would let it
//! leak - `Serialize`, `Display`, `Deref`, or `AsRef`. Each case under
//! `tests/compile_fail/` must fail to compile.

// stderr is pinned to rustc 1.97 diagnostics, so only run under that toolchain.
#[test]
#[rustversion::attr(
    not(stable(1.97)),
    ignore = "trybuild stderr is pinned to rustc 1.97 diagnostics"
)]
fn secret_value_rejects_leaky_traits() {
    trybuild::TestCases::new().compile_fail("tests/compile_fail/*.rs");
}
