//! The seam for the GCP GPU catalog (machine types, accelerators, regions and
//! zones with their prices). The backends take a [`GcpCatalog`]; the data
//! behind it arrives with the catalog work and fills this type in.

/// The GCP GPU catalog. Carries no data yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcpCatalog {}

impl GcpCatalog {
    /// An empty catalog.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}
