//! In-memory endpoint registry: makes running local endpoints addressable and
//! resolves routing aliases to a live, OpenAI-compatible endpoint.
//!
//! A [`RuntimeHandle`]'s `endpoint` is served by llama-server and is
//! OpenAI-compatible (`/v1/chat/completions`, `/v1/models`, ...). This registry
//! only records and resolves those addresses; it does not implement the HTTP
//! server or make any network calls.
//!
//! Resolution is a pure lookup over registered state: an alias selects a
//! [`RoutingPolicy`], whose weighted [`targets`](RoutingPolicy::targets) are
//! filtered to the deployments that currently have a live endpoint. Weighted
//! selection is deterministic - the caller supplies the pick - so routing is
//! testable without a clock or RNG. Active health-probing / failover is a later
//! milestone; the policy's `failover` metadata is carried but not acted on.

use std::collections::HashMap;
use std::sync::RwLock;

use amicu_core::id::{DeploymentId, RoutingPolicyId};
use amicu_core::resource::RoutingPolicy;

use crate::handle::RuntimeHandle;

/// A live target selected during resolution: a deployment, its currently
/// registered OpenAI-compatible endpoint, and its routing weight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    pub deployment: DeploymentId,
    /// The live endpoint, e.g. `http://127.0.0.1:8080`.
    pub endpoint: String,
    /// Relative weight in the policy's traffic split.
    pub weight: u32,
}

/// The outcome of resolving an alias: the matched policy and its live targets.
///
/// The candidate list contains only targets with a registered endpoint, in
/// policy order. It is never empty: the fields are private and the only
/// constructor ([`new`](Self::new)) rejects an empty list, so the non-empty
/// invariant [`weighted`](Self::weighted) relies on cannot be broken from any
/// construction path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    policy: RoutingPolicyId,
    candidates: Vec<ResolvedTarget>,
}

impl Resolved {
    /// Build from a matched policy and its live targets, or `None` when
    /// `candidates` is empty - upholding the non-empty invariant.
    #[must_use]
    fn new(policy: RoutingPolicyId, candidates: Vec<ResolvedTarget>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        Some(Self { policy, candidates })
    }

    /// The matched routing policy.
    #[must_use]
    pub fn policy(&self) -> &RoutingPolicyId {
        &self.policy
    }

    /// The live targets for this alias. Guaranteed non-empty.
    #[must_use]
    pub fn candidates(&self) -> &[ResolvedTarget] {
        &self.candidates
    }

    /// Total weight across live candidates.
    fn total_weight(&self) -> u64 {
        self.candidates.iter().map(|t| u64::from(t.weight)).sum()
    }

    /// Deterministically pick one live target by weight.
    ///
    /// `pick` is supplied by the caller (e.g. a request counter or an RNG draw
    /// made outside this type), keeping selection reproducible in tests. Uses
    /// `pick % total_weight` over the cumulative weights. When every live
    /// candidate has weight `0`, falls back to the first candidate.
    #[must_use]
    pub fn weighted(&self, pick: u64) -> &ResolvedTarget {
        let total = self.total_weight();
        if total == 0 {
            return &self.candidates[0];
        }
        let mut cursor = pick % total;
        for target in &self.candidates {
            let w = u64::from(target.weight);
            if cursor < w {
                return target;
            }
            cursor -= w;
        }
        // Unreachable: cursor < total and weights sum to total.
        &self.candidates[self.candidates.len() - 1]
    }
}

/// Thread-safe registry of live endpoints and routing policies.
///
/// Holds two maps behind [`RwLock`]s: `deployment -> live endpoint` and
/// `routing_policy_id -> policy` (with an alias index). All mutation is
/// idempotent; re-registering updates in place.
#[derive(Debug, Default)]
pub struct EndpointRegistry {
    endpoints: RwLock<HashMap<DeploymentId, String>>,
    policies: RwLock<HashMap<RoutingPolicyId, RoutingPolicy>>,
    aliases: RwLock<HashMap<String, RoutingPolicyId>>,
}

impl EndpointRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the live OpenAI-compatible endpoint for a handle's deployment.
    /// Idempotent: re-registering the same deployment updates its endpoint.
    pub fn register(&self, handle: &RuntimeHandle) {
        self.endpoints
            .write()
            .expect("endpoints lock poisoned")
            .insert(handle.deployment.clone(), handle.endpoint.clone());
    }

    /// Remove a deployment's live endpoint, if present.
    pub fn deregister(&self, deployment: &DeploymentId) {
        self.endpoints
            .write()
            .expect("endpoints lock poisoned")
            .remove(deployment);
    }

    /// The live endpoint currently registered for a deployment, if any.
    #[must_use]
    pub fn endpoint_for(&self, deployment: &DeploymentId) -> Option<String> {
        self.endpoints
            .read()
            .expect("endpoints lock poisoned")
            .get(deployment)
            .cloned()
    }

    /// Register (or replace) a routing policy and index its aliases.
    /// Idempotent on the policy id; stale aliases from a prior version are
    /// dropped before the new ones are indexed.
    pub fn register_policy(&self, policy: RoutingPolicy) {
        let mut policies = self.policies.write().expect("policies lock poisoned");
        let mut aliases = self.aliases.write().expect("aliases lock poisoned");
        if let Some(prev) = policies.get(&policy.id) {
            for alias in &prev.aliases {
                if aliases.get(alias) == Some(&policy.id) {
                    aliases.remove(alias);
                }
            }
        }
        for alias in &policy.aliases {
            aliases.insert(alias.clone(), policy.id.clone());
        }
        policies.insert(policy.id.clone(), policy);
    }

    /// Remove a routing policy and its alias index entries.
    pub fn deregister_policy(&self, id: &RoutingPolicyId) {
        let mut policies = self.policies.write().expect("policies lock poisoned");
        let mut aliases = self.aliases.write().expect("aliases lock poisoned");
        if let Some(policy) = policies.remove(id) {
            for alias in &policy.aliases {
                if aliases.get(alias) == Some(id) {
                    aliases.remove(alias);
                }
            }
        }
    }

    /// Resolve an alias to its policy's live targets.
    ///
    /// Returns `None` when no policy claims the alias or when none of the
    /// policy's targets currently have a registered endpoint.
    #[must_use]
    pub fn resolve(&self, alias: &str) -> Option<Resolved> {
        let policy_id = self
            .aliases
            .read()
            .expect("aliases lock poisoned")
            .get(alias)
            .cloned()?;
        let policies = self.policies.read().expect("policies lock poisoned");
        let policy = policies.get(&policy_id)?;
        let endpoints = self.endpoints.read().expect("endpoints lock poisoned");
        let candidates: Vec<ResolvedTarget> = policy
            .targets
            .iter()
            .filter_map(|t| {
                endpoints.get(&t.deployment).map(|endpoint| ResolvedTarget {
                    deployment: t.deployment.clone(),
                    endpoint: endpoint.clone(),
                    weight: t.weight,
                })
            })
            .collect();
        Resolved::new(policy_id, candidates)
    }

    /// Resolve an alias and deterministically pick one live target by weight.
    ///
    /// Convenience over [`resolve`](Self::resolve) + [`Resolved::weighted`].
    #[must_use]
    pub fn resolve_weighted(&self, alias: &str, pick: u64) -> Option<ResolvedTarget> {
        self.resolve(alias).map(|r| r.weighted(pick).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amicu_core::resource::RoutingTarget;

    fn handle(deployment: &str, endpoint: &str) -> RuntimeHandle {
        RuntimeHandle {
            id: format!("i-{deployment}"),
            deployment: DeploymentId::new(deployment),
            endpoint: endpoint.to_owned(),
        }
    }

    fn policy(id: &str, aliases: &[&str], targets: &[(&str, u32)]) -> RoutingPolicy {
        RoutingPolicy {
            id: RoutingPolicyId::new(id),
            aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
            targets: targets
                .iter()
                .map(|(d, w)| RoutingTarget {
                    deployment: DeploymentId::new(*d),
                    weight: *w,
                })
                .collect(),
            failover: None,
            optimize_for: None,
        }
    }

    #[test]
    fn register_and_direct_lookup() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        assert_eq!(
            reg.endpoint_for(&DeploymentId::new("chat_local"))
                .as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert!(reg.endpoint_for(&DeploymentId::new("missing")).is_none());
    }

    #[test]
    fn register_is_idempotent() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        reg.register(&handle("chat_local", "http://127.0.0.1:9090"));
        assert_eq!(
            reg.endpoint_for(&DeploymentId::new("chat_local"))
                .as_deref(),
            Some("http://127.0.0.1:9090")
        );
    }

    #[test]
    fn deregister_removes_endpoint() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        reg.deregister(&DeploymentId::new("chat_local"));
        assert!(reg.endpoint_for(&DeploymentId::new("chat_local")).is_none());
    }

    #[test]
    fn resolve_alias_to_live_endpoint() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        reg.register_policy(policy(
            "chat",
            &["gpt-4o", "assistant"],
            &[("chat_local", 1)],
        ));

        let resolved = reg.resolve("gpt-4o").expect("alias resolves");
        assert_eq!(resolved.policy(), &RoutingPolicyId::new("chat"));
        assert_eq!(resolved.candidates().len(), 1);
        assert_eq!(resolved.candidates()[0].endpoint, "http://127.0.0.1:8080");

        // Second alias for the same policy also resolves.
        assert_eq!(
            reg.resolve("assistant").unwrap().candidates()[0].endpoint,
            "http://127.0.0.1:8080"
        );
        assert!(reg.resolve("unknown").is_none());
    }

    #[test]
    fn resolve_skips_targets_without_live_endpoint() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("primary", "http://127.0.0.1:8080"));
        // `backup` has no registered endpoint.
        reg.register_policy(policy("chat", &["chat"], &[("primary", 3), ("backup", 1)]));

        let resolved = reg.resolve("chat").expect("resolves");
        assert_eq!(resolved.candidates().len(), 1);
        assert_eq!(
            resolved.candidates()[0].deployment,
            DeploymentId::new("primary")
        );
    }

    #[test]
    fn resolve_none_when_nothing_live() {
        let reg = EndpointRegistry::new();
        reg.register_policy(policy("chat", &["chat"], &[("primary", 1)]));
        assert!(reg.resolve("chat").is_none());
    }

    #[test]
    fn weighted_selection_is_deterministic() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("a", "http://a:8080"));
        reg.register(&handle("b", "http://b:8080"));
        reg.register_policy(policy("split", &["chat"], &[("a", 3), ("b", 1)]));

        let resolved = reg.resolve("chat").unwrap();
        // Cumulative weights: a=[0,3), b=[3,4). pick % 4.
        assert_eq!(resolved.weighted(0).endpoint, "http://a:8080");
        assert_eq!(resolved.weighted(2).endpoint, "http://a:8080");
        assert_eq!(resolved.weighted(3).endpoint, "http://b:8080");
        assert_eq!(resolved.weighted(7).endpoint, "http://b:8080"); // 7 % 4 == 3

        assert_eq!(
            reg.resolve_weighted("chat", 3).unwrap().endpoint,
            "http://b:8080"
        );
    }

    #[test]
    fn weighted_all_zero_falls_back_to_first() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("a", "http://a:8080"));
        reg.register(&handle("b", "http://b:8080"));
        reg.register_policy(policy("split", &["chat"], &[("a", 0), ("b", 0)]));
        assert_eq!(
            reg.resolve("chat").unwrap().weighted(5).endpoint,
            "http://a:8080"
        );
    }

    #[test]
    fn resolved_rejects_empty_candidates() {
        // The non-empty invariant is type-enforced: no `Resolved` can be built
        // with empty candidates, so `weighted` can never index-panic on one.
        assert!(Resolved::new(RoutingPolicyId::new("chat"), Vec::new()).is_none());
    }

    #[test]
    fn deregister_policy_clears_aliases() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        reg.register_policy(policy("chat", &["gpt-4o"], &[("chat_local", 1)]));
        reg.deregister_policy(&RoutingPolicyId::new("chat"));
        assert!(reg.resolve("gpt-4o").is_none());
    }

    #[test]
    fn re_registering_policy_drops_stale_aliases() {
        let reg = EndpointRegistry::new();
        reg.register(&handle("chat_local", "http://127.0.0.1:8080"));
        reg.register_policy(policy("chat", &["old-name"], &[("chat_local", 1)]));
        reg.register_policy(policy("chat", &["new-name"], &[("chat_local", 1)]));
        assert!(reg.resolve("old-name").is_none());
        assert!(reg.resolve("new-name").is_some());
    }
}
