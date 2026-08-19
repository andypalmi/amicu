// SecretValue must not implement AsRef<[u8]>: exposure must stay explicit.
fn assert_as_ref<T: AsRef<[u8]>>() {}

fn main() {
    assert_as_ref::<amicu_core::secret::SecretValue>();
}
