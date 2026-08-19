//! Typed adapter registry and capability discovery.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{Adapter, AdapterId, DescribeCapabilities};

/// A registry of adapters of one family, keyed by [`AdapterId`].
///
/// Generic over the family trait object `T` (e.g. `Registry<dyn
/// RuntimeAdapter>`) so each adapter family gets its own typed registry while
/// sharing this implementation. Adapters are held as `Arc<T>` and dispatched
/// dynamically at runtime.
pub struct Registry<T: ?Sized> {
    adapters: HashMap<AdapterId, Arc<T>>,
}

impl<T: ?Sized> Default for Registry<T> {
    fn default() -> Self {
        Self {
            adapters: HashMap::new(),
        }
    }
}

impl<T: ?Sized> Registry<T> {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of registered adapters.
    #[must_use]
    pub fn len(&self) -> usize {
        self.adapters.len()
    }

    /// Whether the registry holds no adapters.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Look up an adapter by id.
    #[must_use]
    pub fn get(&self, id: &AdapterId) -> Option<Arc<T>> {
        self.adapters.get(id).cloned()
    }

    /// Every registered adapter id.
    pub fn ids(&self) -> impl Iterator<Item = &AdapterId> {
        self.adapters.keys()
    }

    /// Every registered adapter, in unspecified order.
    #[must_use]
    pub fn list(&self) -> Vec<Arc<T>> {
        self.adapters.values().cloned().collect()
    }
}

impl<T: Adapter + ?Sized> Registry<T> {
    /// Register an adapter under its own [`Adapter::id`], returning any adapter
    /// previously registered under the same id.
    pub fn register(&mut self, adapter: Arc<T>) -> Option<Arc<T>> {
        self.adapters.insert(adapter.id(), adapter)
    }
}

impl<T: Adapter + DescribeCapabilities + ?Sized> Registry<T> {
    /// List every adapter id alongside its plain-data capability descriptor.
    ///
    /// Discovery is side-effect-free: it reads each adapter's declared
    /// capabilities without provisioning or reaching out to any backend.
    #[must_use]
    pub fn discover(&self) -> Vec<AdapterDescriptor> {
        self.adapters
            .values()
            .map(|a| AdapterDescriptor {
                id: a.id(),
                capabilities: a.describe(),
            })
            .collect()
    }
}

/// An entry produced by [`Registry::discover`]: an adapter id and its
/// serde-serialized capability descriptor.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AdapterDescriptor {
    /// The adapter's stable id.
    pub id: AdapterId,
    /// The adapter's plain-data capability descriptor.
    pub capabilities: serde_json::Value,
}
