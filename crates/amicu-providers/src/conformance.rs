//! Conformance harness for the orphan-proof provider contract.
//!
//! Every adapter that can create billable resources must pass [`run`] before it
//! is wired to `apply` (andypalmi/amicu-planning#215). The harness drives an
//! adapter through a stateful mock backend (a [`Fixture`]) and checks, with
//! fault injection, that:
//!
//! 1. `prepare` creates nothing (record exists before any resource) and the
//!    record round-trips through serde without leaking the secret;
//! 2. teardown works from the serialized record alone, on a provider with no
//!    adapter-level configuration;
//! 3. teardown is idempotent (absent = confirmed gone);
//! 4. an unconfirmed or failed stop is never reported as confirmed;
//! 5. a crash between create and record (response lost) still leaves a
//!    record that stops the resource, via the deterministic identity;
//! 6. re-running `provision` for the same record adopts, never duplicates;
//! 7. a failure after the create call still leaves a stoppable record.
//!
//! Adding an adapter: implement [`Fixture`] over a mock backend that honours
//! the [`Fault`]s, and call [`run`] from a test. Catalog-only adapters (no
//! apply-time backend yet) have no fixture and are skipped by design.

#![allow(clippy::match_wildcard_for_single_variants, clippy::single_match_else)]

use std::sync::{Arc, Mutex};

use amicu_core::adapter::AdapterError;
use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use async_trait::async_trait;

use crate::provider::{
    Provider, ProvisionRequest, ProvisioningHandle, ResourceIdentity, TeardownOutcome,
};

/// The marker secret the harness resolves; it must never appear in a record.
const SECRET: &str = "s3cr3t-conformance-token";

/// A fault a mock backend can be armed with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Fault {
    /// The next create succeeds server-side but the response is lost (crash
    /// between create and record).
    CreateResponseLost,
    /// Delete calls fail.
    DeleteFails,
    /// Delete calls are accepted but the resource lingers.
    DeleteLingers,
    /// A step after the create call fails (multi-step providers only).
    PostCreateStepFails,
}

/// A mock backend plus the adapter wired to it.
pub(crate) trait Fixture {
    /// The adapter under test.
    type P: Provider;
    /// The fully configured adapter.
    fn provider(&self) -> Self::P;
    /// An adapter over the same backend with no adapter-level configuration
    /// (no declared hosts, no cached state): proves teardown needs the record
    /// only.
    fn bare_provider(&self) -> Self::P;
    /// A valid request for the adapter.
    fn request(&self) -> ProvisionRequest;
    /// Resources currently existing in the mock backend.
    fn live_resources(&self) -> usize;
    /// Arm or disarm a fault.
    fn set_fault(&self, fault: Fault, on: bool);
    /// Whether the fixture can inject `fault`.
    fn supports(&self, fault: Fault) -> bool;
}

struct Resolver;

#[async_trait]
impl SecretResolver for Resolver {
    async fn resolve(&self, secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        if secret.name == "cred" {
            Ok(SecretValue::new(SECRET.to_string()))
        } else {
            Err(AdapterError::permanent(
                DiagnosticCode::new(DiagCategory::Policy, 10),
                "secret unavailable",
            ))
        }
    }
}

fn identity() -> ResourceIdentity {
    ResourceIdentity::new("deployment-1", "project-1")
}

fn round_trip(record: &ProvisioningHandle) -> ProvisioningHandle {
    let json = serde_json::to_string(record).expect("record serializes");
    assert!(!json.contains(SECRET), "record must not contain a secret");
    serde_json::from_str(&json).expect("record deserializes")
}

/// Run every conformance scenario against fresh fixtures from `make`.
pub(crate) async fn run<F: Fixture>(make: impl Fn() -> F) {
    prepare_creates_nothing_and_record_is_self_describing(&make()).await;
    teardown_from_record_only(&make()).await;
    teardown_is_idempotent(&make()).await;
    unconfirmed_is_not_success(&make()).await;
    crash_between_create_and_record_is_stoppable(&make()).await;
    provision_adopts_instead_of_duplicating(&make()).await;
    failure_after_create_is_stoppable(&make()).await;
}

fn req<F: Fixture>(f: &F) -> ProvisionRequest {
    f.request().with_credentials(SecretRef::new("cred"))
}

async fn prepare_creates_nothing_and_record_is_self_describing<F: Fixture>(f: &F) {
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).expect("prepare");
    // Write-ahead: the record exists while the backend holds nothing.
    assert_eq!(f.live_resources(), 0, "prepare must not create resources");
    assert!(!prepared.has_resource_id());
    assert_eq!(prepared.identity, identity());
    assert_eq!(prepared.provider, p.capabilities().provider);
    assert_eq!(prepared.credentials, Some(SecretRef::new("cred")));
    assert_eq!(round_trip(&prepared), prepared);

    let done = p
        .provision(prepared.clone(), &Resolver)
        .await
        .expect("provision");
    assert_eq!(f.live_resources(), 1);
    assert!(done.has_resource_id(), "provision records the resource id");
    assert_eq!(done.identity, prepared.identity);
    assert_eq!(round_trip(&done), done);
}

async fn teardown_from_record_only<F: Fixture>(f: &F) {
    let prepared = f.provider().prepare(&req(f), identity()).unwrap();
    let done = f.provider().provision(prepared, &Resolver).await.unwrap();
    let stored = round_trip(&done);
    let outcome = f
        .bare_provider()
        .teardown(&stored, &Resolver)
        .await
        .expect("teardown from record alone");
    assert_eq!(outcome, TeardownOutcome::ConfirmedGone);
    assert_eq!(f.live_resources(), 0, "teardown must remove the resource");
}

async fn teardown_is_idempotent<F: Fixture>(f: &F) {
    let p = f.provider();
    // Never created: absent counts as confirmed gone.
    let prepared = p.prepare(&req(f), identity()).unwrap();
    assert_eq!(
        p.teardown(&prepared, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    let done = p.provision(prepared, &Resolver).await.unwrap();
    for _ in 0..2 {
        assert_eq!(
            p.teardown(&done, &Resolver).await.unwrap(),
            TeardownOutcome::ConfirmedGone
        );
    }
    assert_eq!(f.live_resources(), 0);
}

async fn unconfirmed_is_not_success<F: Fixture>(f: &F) {
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).unwrap();
    let done = p.provision(prepared, &Resolver).await.unwrap();

    if f.supports(Fault::DeleteLingers) {
        f.set_fault(Fault::DeleteLingers, true);
        let outcome = p.teardown(&done, &Resolver).await;
        assert!(
            !matches!(outcome, Ok(TeardownOutcome::ConfirmedGone)),
            "a lingering resource must not be confirmed gone"
        );
        assert_eq!(f.live_resources(), 1);
        f.set_fault(Fault::DeleteLingers, false);
    }
    if f.supports(Fault::DeleteFails) {
        f.set_fault(Fault::DeleteFails, true);
        let outcome = p.teardown(&done, &Resolver).await;
        assert!(
            !matches!(outcome, Ok(TeardownOutcome::ConfirmedGone)),
            "a failed delete must not be confirmed gone"
        );
        f.set_fault(Fault::DeleteFails, false);
    }
    // Once the backend recovers, the retained record still stops it.
    assert_eq!(
        p.teardown(&done, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 0);
}

async fn crash_between_create_and_record_is_stoppable<F: Fixture>(f: &F) {
    assert!(f.supports(Fault::CreateResponseLost));
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).unwrap();
    let persisted = round_trip(&prepared); // all the caller has after a crash
    f.set_fault(Fault::CreateResponseLost, true);
    assert!(p.provision(prepared, &Resolver).await.is_err());
    assert_eq!(
        f.live_resources(),
        1,
        "the resource exists but no id was recorded"
    );
    assert!(!persisted.has_resource_id());

    let outcome = f
        .bare_provider()
        .teardown(&persisted, &Resolver)
        .await
        .unwrap();
    assert_eq!(outcome, TeardownOutcome::ConfirmedGone);
    assert_eq!(
        f.live_resources(),
        0,
        "identity lookup must find the orphan"
    );
}

async fn provision_adopts_instead_of_duplicating<F: Fixture>(f: &F) {
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).unwrap();
    f.set_fault(Fault::CreateResponseLost, true);
    assert!(p.provision(prepared.clone(), &Resolver).await.is_err());
    let retried = p.provision(prepared, &Resolver).await.unwrap();
    assert_eq!(f.live_resources(), 1, "retry must adopt, not duplicate");
    assert_eq!(
        p.teardown(&retried, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 0);
}

async fn failure_after_create_is_stoppable<F: Fixture>(f: &F) {
    if !f.supports(Fault::PostCreateStepFails) {
        return;
    }
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).unwrap();
    f.set_fault(Fault::PostCreateStepFails, true);
    assert!(p.provision(prepared.clone(), &Resolver).await.is_err());
    assert_eq!(f.live_resources(), 1);
    assert_eq!(
        p.teardown(&prepared, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(
        f.live_resources(),
        0,
        "no warn-and-drop: the server is gone"
    );
}

// ---------------------------------------------------------------------------
// Stateful mock backends
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Faults(std::collections::HashSet<Fault>);

impl Faults {
    fn set(&mut self, fault: Fault, on: bool) {
        if on {
            self.0.insert(fault);
        } else {
            self.0.remove(&fault);
        }
    }

    fn has(&self, fault: Fault) -> bool {
        self.0.contains(&fault)
    }
}

#[derive(Default)]
struct Store {
    /// `(id, name)` of every live resource.
    resources: Vec<(String, String)>,
    next: u32,
    faults: Faults,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    Hetzner,
    RunPod,
    Scaleway,
    Ovh,
}

impl Dialect {
    fn collection(self) -> &'static str {
        match self {
            Dialect::Hetzner | Dialect::Scaleway => "servers",
            Dialect::RunPod => "pods",
            Dialect::Ovh => "instance",
        }
    }

    fn new_id(self, n: u32) -> String {
        match self {
            Dialect::Hetzner => n.to_string(),
            Dialect::RunPod => format!("pod-{n}"),
            Dialect::Scaleway => format!("srv-{n}"),
            Dialect::Ovh => format!("ins-{n}"),
        }
    }

    fn object(self, id: &str, name: &str) -> serde_json::Value {
        use serde_json::json;
        match self {
            Dialect::Hetzner => {
                json!({"id": id.parse::<i64>().unwrap(), "name": name, "status": "running"})
            }
            Dialect::RunPod => json!({"id": id, "name": name, "desiredStatus": "RUNNING"}),
            Dialect::Scaleway => json!({"id": id, "name": name, "state": "stopped"}),
            Dialect::Ovh => json!({"id": id, "name": name, "status": "ACTIVE"}),
        }
    }

    fn one(self, obj: serde_json::Value) -> serde_json::Value {
        match self {
            Dialect::Hetzner | Dialect::Scaleway => serde_json::json!({ "server": obj }),
            Dialect::RunPod | Dialect::Ovh => obj,
        }
    }

    fn list(self, objs: Vec<serde_json::Value>) -> serde_json::Value {
        match self {
            Dialect::Hetzner | Dialect::Scaleway => serde_json::json!({ "servers": objs }),
            Dialect::RunPod | Dialect::Ovh => serde_json::Value::Array(objs),
        }
    }
}

/// A stateful mock cloud REST API.
struct HttpCloud {
    dialect: Dialect,
    base: &'static str,
    store: Arc<Mutex<Store>>,
}

fn json_response(status: u16, value: &serde_json::Value) -> crate::transport::HttpResponse {
    crate::transport::HttpResponse::new(status, serde_json::to_vec(value).unwrap())
}

#[async_trait]
impl crate::transport::HttpTransport for HttpCloud {
    async fn execute(
        &self,
        request: crate::transport::HttpRequest,
    ) -> Result<crate::transport::HttpResponse, crate::transport::TransportError> {
        use crate::transport::{HttpMethod, HttpResponse, TransportError};
        let rest = request.url.strip_prefix(self.base).expect("mock base url");
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let name_filter = query.strip_prefix("name=");
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let coll = self.dialect.collection();
        let d = self.dialect;
        let mut st = self.store.lock().unwrap();

        let last = *segs.last().unwrap();
        let prev = if segs.len() >= 2 {
            segs[segs.len() - 2]
        } else {
            ""
        };
        let prev2 = if segs.len() >= 3 {
            segs[segs.len() - 3]
        } else {
            ""
        };

        // POST .../servers/{id}/action
        if request.method == HttpMethod::Post && last == "action" && prev2 == coll {
            if st.faults.has(Fault::PostCreateStepFails) {
                return Ok(HttpResponse::new(500, Vec::new()));
            }
            return Ok(json_response(202, &serde_json::json!({})));
        }
        // collection
        if last == coll {
            return match request.method {
                HttpMethod::Post => {
                    let name = request
                        .body
                        .as_ref()
                        .and_then(|b| b.get("name"))
                        .and_then(|n| n.as_str())
                        .expect("create body carries the identity name")
                        .to_string();
                    st.next += 1;
                    let id = d.new_id(st.next);
                    st.resources.push((id.clone(), name.clone()));
                    if st.faults.has(Fault::CreateResponseLost) {
                        st.faults.set(Fault::CreateResponseLost, false);
                        return Err(TransportError::Timeout);
                    }
                    Ok(json_response(201, &d.one(d.object(&id, &name))))
                }
                HttpMethod::Get => {
                    let objs = st
                        .resources
                        .iter()
                        .filter(|(_, n)| name_filter.is_none_or(|f| f == n))
                        .map(|(i, n)| d.object(i, n))
                        .collect();
                    Ok(json_response(200, &d.list(objs)))
                }
                _ => Ok(HttpResponse::new(405, Vec::new())),
            };
        }
        // item
        if prev == coll {
            let found = st.resources.iter().position(|(i, _)| i == last);
            return match (request.method, found) {
                (HttpMethod::Get, Some(ix)) => {
                    let (i, n) = st.resources[ix].clone();
                    Ok(json_response(200, &d.one(d.object(&i, &n))))
                }
                (HttpMethod::Delete, _) if st.faults.has(Fault::DeleteFails) => {
                    Ok(HttpResponse::new(500, Vec::new()))
                }
                (HttpMethod::Delete, Some(ix)) => {
                    if !st.faults.has(Fault::DeleteLingers) {
                        st.resources.remove(ix);
                    }
                    Ok(json_response(200, &serde_json::json!({})))
                }
                (_, None) => Ok(HttpResponse::new(404, Vec::new())),
                _ => Ok(HttpResponse::new(405, Vec::new())),
            };
        }
        Ok(HttpResponse::new(404, Vec::new()))
    }
}

/// A stateful mock of the self-hosted host agent behind SSH.
struct AgentCloud {
    store: Arc<Mutex<Store>>,
}

#[async_trait]
impl crate::ssh::SshTransport for AgentCloud {
    async fn run(
        &self,
        _target: &crate::ssh::SshTarget,
        command: &str,
        _key: &SecretValue,
    ) -> Result<crate::ssh::CommandOutput, crate::ssh::SshTransportError> {
        use crate::ssh::{CommandOutput, SshTransportError};
        let words: Vec<String> = command
            .split_whitespace()
            .map(|w| w.trim_matches('\'').to_string())
            .collect();
        let verb = words[1].as_str();
        let by_name = words.get(2).is_some_and(|w| w == "--name");
        let mut st = self.store.lock().unwrap();
        let ok = |s: &str| Ok(CommandOutput::new(0, s.as_bytes().to_vec(), Vec::new()));
        let not_found = Ok(CommandOutput::new(44, Vec::new(), Vec::new()));
        if verb == "provision" {
            let name = words[words.iter().position(|w| w == "--name").unwrap() + 1].clone();
            let id = match st.resources.iter().find(|(_, n)| *n == name) {
                Some((i, _)) => i.clone(),
                None => {
                    st.next += 1;
                    let id = format!("agent-{}", st.next);
                    st.resources.push((id.clone(), name));
                    id
                }
            };
            if st.faults.has(Fault::CreateResponseLost) {
                st.faults.set(Fault::CreateResponseLost, false);
                return Err(SshTransportError::Timeout);
            }
            return ok(&id);
        }
        let key = if by_name { &words[3] } else { &words[2] };
        let found = st
            .resources
            .iter()
            .position(|(i, n)| if by_name { n == key } else { i == key });
        match (verb, found) {
            ("status", Some(_)) => ok("ready"),
            ("teardown", _) if st.faults.has(Fault::DeleteFails) => {
                Ok(CommandOutput::new(1, Vec::new(), b"boom".to_vec()))
            }
            ("teardown", Some(ix)) => {
                if !st.faults.has(Fault::DeleteLingers) {
                    st.resources.remove(ix);
                }
                ok("")
            }
            (_, None) => not_found,
            _ => Ok(CommandOutput::new(2, Vec::new(), b"bad verb".to_vec())),
        }
    }
}

struct HttpFixture<P> {
    store: Arc<Mutex<Store>>,
    dialect: Dialect,
    base: &'static str,
    build: fn(Arc<dyn crate::transport::HttpTransport>, &'static str) -> P,
    offering: &'static str,
    region: &'static str,
}

impl<P: Provider> Fixture for HttpFixture<P> {
    type P = P;

    fn provider(&self) -> P {
        let transport: Arc<dyn crate::transport::HttpTransport> = Arc::new(HttpCloud {
            dialect: self.dialect,
            base: self.base,
            store: self.store.clone(),
        });
        (self.build)(transport, self.base)
    }

    fn bare_provider(&self) -> P {
        self.provider()
    }

    fn request(&self) -> ProvisionRequest {
        ProvisionRequest::new(self.offering, self.region)
    }

    fn live_resources(&self) -> usize {
        self.store.lock().unwrap().resources.len()
    }

    fn set_fault(&self, fault: Fault, on: bool) {
        self.store.lock().unwrap().faults.set(fault, on);
    }

    fn supports(&self, fault: Fault) -> bool {
        fault != Fault::PostCreateStepFails || self.dialect == Dialect::Scaleway
    }
}

struct SelfHostedFixture {
    store: Arc<Mutex<Store>>,
}

impl SelfHostedFixture {
    fn agent(&self) -> Arc<dyn crate::ssh::SshTransport> {
        Arc::new(AgentCloud {
            store: self.store.clone(),
        })
    }
}

impl Fixture for SelfHostedFixture {
    type P = crate::self_hosted::SelfHostedProvider;

    fn provider(&self) -> Self::P {
        use crate::capability::{Region, Residency};
        crate::self_hosted::SelfHostedProvider::new()
            .with_host(crate::self_hosted::DeclaredHost::new(
                "rack-de-01",
                Region::new("dc-frankfurt", Residency::Eu),
            ))
            .with_transport(self.agent())
    }

    fn bare_provider(&self) -> Self::P {
        // No declared hosts: teardown must rely on the record's SSH target.
        crate::self_hosted::SelfHostedProvider::new().with_transport(self.agent())
    }

    fn request(&self) -> ProvisionRequest {
        ProvisionRequest::new("rack-de-01", "dc-frankfurt")
    }

    fn live_resources(&self) -> usize {
        self.store.lock().unwrap().resources.len()
    }

    fn set_fault(&self, fault: Fault, on: bool) {
        self.store.lock().unwrap().faults.set(fault, on);
    }

    fn supports(&self, fault: Fault) -> bool {
        fault != Fault::PostCreateStepFails
    }
}

fn http_fixture<P>(
    dialect: Dialect,
    base: &'static str,
    build: fn(Arc<dyn crate::transport::HttpTransport>, &'static str) -> P,
    offering: &'static str,
    region: &'static str,
) -> HttpFixture<P> {
    HttpFixture {
        store: Arc::default(),
        dialect,
        base,
        build,
        offering,
        region,
    }
}

#[cfg(test)]
mod adapters {
    use super::*;

    #[tokio::test]
    async fn hetzner_conforms() {
        run(|| {
            http_fixture(
                Dialect::Hetzner,
                "https://mock.test/v1",
                |t, b| crate::HetznerProvider::new().with_transport(t, b),
                "gpu.rtx-4000-ada.1x",
                "nbg1",
            )
        })
        .await;
    }

    #[tokio::test]
    async fn runpod_conforms() {
        run(|| {
            http_fixture(
                Dialect::RunPod,
                "https://mock.test/v1",
                |t, b| crate::RunPodProvider::new().with_transport(t, b),
                "gpu.a100-80gb.1x",
                "EU-RO-1",
            )
        })
        .await;
    }

    #[tokio::test]
    async fn scaleway_conforms() {
        run(|| {
            http_fixture(
                Dialect::Scaleway,
                "https://mock.test/instance/v1",
                |t, b| crate::ScalewayProvider::new().with_transport(t, b),
                "gpu.l40s.1x",
                "fr-par",
            )
        })
        .await;
    }

    #[tokio::test]
    async fn ovhcloud_conforms() {
        run(|| {
            http_fixture(
                Dialect::Ovh,
                "https://mock.test/v1",
                |t, b| crate::OvhCloudProvider::new().with_transport(t, b),
                OVH_OFFERING,
                OVH_REGION,
            )
        })
        .await;
    }

    #[tokio::test]
    async fn self_hosted_conforms() {
        run(|| SelfHostedFixture {
            store: Arc::default(),
        })
        .await;
    }

    // Catalog-only adapters (aws, azure, gcp, nebius, modal) have no apply-time
    // backend yet, so they have no fixture and are skipped. Wiring one to
    // `apply` requires adding its fixture here first.

    const OVH_OFFERING: &str = "gpu.a100.1x";
    const OVH_REGION: &str = "GRA";
}
