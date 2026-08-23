//! Stable, serde-friendly identifiers for canonical resources.
//!
//! Each resource kind has its own newtype id so that references between
//! resources are checked by the type system (a [`ModelId`] cannot stand in
//! for a [`HardwareProfileId`]). Every id serializes transparently as the
//! underlying string, matching the DSL/JSON form (e.g. `"support_chat"`).

use serde::{Deserialize, Serialize};
use std::fmt;

/// Discriminates the ten canonical resource kinds. Useful for generic
/// addressing and for reporting dangling references by kind.
///
/// `Credential` (amicu-planning#119) is a named bundle of secret-designated
/// fields (`api_key`, `token`, `password`, an `env` map); it holds only
/// [`crate::secret::SecretRef`] pointers, never secret bytes, so it carries no
/// outgoing references and is exempt from [`crate::graph::Graph::missing_references`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    Model,
    HardwareProfile,
    Deployment,
    RoutingPolicy,
    DataClass,
    GovernanceRule,
    Budget,
    Tenant,
    UseCase,
    Credential,
}

impl ResourceKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::HardwareProfile => "hardware_profile",
            Self::Deployment => "deployment",
            Self::RoutingPolicy => "routing_policy",
            Self::DataClass => "data_class",
            Self::GovernanceRule => "governance_rule",
            Self::Budget => "budget",
            Self::Tenant => "tenant",
            Self::UseCase => "use_case",
            Self::Credential => "credential",
        }
    }
}

impl fmt::Display for ResourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! resource_id {
    ($(#[$meta:meta])* $name:ident => $kind:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub const KIND: ResourceKind = $kind;

            #[must_use]
            pub fn new(id: impl Into<String>) -> Self {
                Self(id.into())
            }

            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(id: String) -> Self {
                Self(id)
            }
        }

        impl From<&str> for $name {
            fn from(id: &str) -> Self {
                Self(id.to_string())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

resource_id!(
    /// Identifier for a `model` resource.
    ModelId => ResourceKind::Model
);
resource_id!(
    /// Identifier for a `hardware_profile` resource.
    HardwareProfileId => ResourceKind::HardwareProfile
);
resource_id!(
    /// Identifier for a `deployment` resource.
    DeploymentId => ResourceKind::Deployment
);
resource_id!(
    /// Identifier for a `routing_policy` resource.
    RoutingPolicyId => ResourceKind::RoutingPolicy
);
resource_id!(
    /// Identifier for a `data_class` resource.
    DataClassId => ResourceKind::DataClass
);
resource_id!(
    /// Identifier for a `governance_rule` resource.
    GovernanceRuleId => ResourceKind::GovernanceRule
);
resource_id!(
    /// Identifier for a `budget` resource.
    BudgetId => ResourceKind::Budget
);
resource_id!(
    /// Identifier for a `tenant` resource.
    TenantId => ResourceKind::Tenant
);
resource_id!(
    /// Identifier for a `use_case` resource.
    UseCaseId => ResourceKind::UseCase
);
resource_id!(
    /// Identifier for a `credential` resource.
    CredentialId => ResourceKind::Credential
);

/// A resource reference that has failed existence checking, reported by
/// [`crate::graph::Graph::missing_references`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingReference {
    /// Kind and id of the resource that holds the dangling reference.
    pub from_kind: ResourceKind,
    pub from_id: String,
    /// Kind and id of the referenced resource that does not exist.
    pub to_kind: ResourceKind,
    pub to_id: String,
}

impl fmt::Display for MissingReference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} `{}` references missing {} `{}`",
            self.from_kind, self.from_id, self.to_kind, self.to_id
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_serialize_transparently() {
        let id = ModelId::new("support_chat");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"support_chat\"");
        assert_eq!(
            serde_json::from_str::<ModelId>("\"support_chat\"").unwrap(),
            id
        );
    }

    #[test]
    fn kind_is_snake_case() {
        assert_eq!(HardwareProfileId::KIND.as_str(), "hardware_profile");
        assert_eq!(
            serde_json::to_string(&ResourceKind::RoutingPolicy).unwrap(),
            "\"routing_policy\""
        );
    }
}
