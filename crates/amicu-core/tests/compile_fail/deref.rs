// SecretValue must not implement Deref: bytes are reachable only via
// the explicitly named expose_secret accessors.
fn assert_deref<T: std::ops::Deref>() {}

fn main() {
    assert_deref::<amicu_core::secret::SecretValue>();
}
