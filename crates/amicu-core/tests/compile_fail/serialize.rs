// SecretValue must not implement serde::Serialize.
fn assert_serialize<T: serde::Serialize>() {}

fn main() {
    assert_serialize::<amicu_core::secret::SecretValue>();
}
