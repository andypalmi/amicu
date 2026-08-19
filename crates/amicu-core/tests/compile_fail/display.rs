// SecretValue must not implement Display (nor, by extension, ToString).
fn assert_display<T: std::fmt::Display>() {}

fn main() {
    assert_display::<amicu_core::secret::SecretValue>();
}
