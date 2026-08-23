// A secret-bearing field in a persisted-state type must be a `SecretRef`
// (a pointer), never a `SecretValue` (the resolved plaintext). This mirrors
// exactly how `StateSnapshot`'s `checksum_of` persists a graph: serialize it
// to canonical JSON. If a future field ever holds a `SecretValue`, that
// serialization must fail to compile here, not leak into a stored snapshot.

use amicu_core::secret::SecretValue;
use serde::Serialize;

#[derive(Serialize)]
struct PersistedResource {
    id: String,
    credential: SecretValue,
}

fn main() {
    let resource = PersistedResource {
        id: "example".to_string(),
        credential: SecretValue::new("plaintext-secret"),
    };
    let _ = serde_json::to_string(&resource);
}
