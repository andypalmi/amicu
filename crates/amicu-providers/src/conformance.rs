//! Conformance harness for the orphan-proof provider contract.
//!
//! Every adapter that can create billable resources must pass [`run`] before it
//! is wired to `apply`. The harness drives an
//! adapter through a stateful mock backend (a [`Fixture`]) and checks, with
//! fault injection, that:
//!
//! 1. `prepare` creates nothing (record exists before any resource) and the
//!    record round-trips through serde without leaking the secret;
//! 2. teardown works from the serialized record alone, on a provider with no
//!    adapter-level configuration;
//! 3. teardown is idempotent (absent = confirmed gone, where the backend can
//!    vouch for absence);
//! 4. an unconfirmed or failed stop is never reported as confirmed;
//! 5. a crash between create and record (response lost) still leaves a
//!    record that stops the resource, via the identity;
//! 6. re-running `provision` for the same record adopts, never duplicates, and
//!    a stopped resource left by a crash ends up running;
//! 7. a failure after the create call still leaves a stoppable record, and a
//!    retry recovers;
//! 8. other identities' resources (including many, past one listing page) are
//!    neither adopted nor deleted;
//! 9. adoption refuses a resource with a different offering/region and refuses
//!    to guess among several;
//! 10. a 404 at the wrong scope (zone, project, account, host) is never
//!     "confirmed gone";
//! 11. tampered records (bad names/ids, forged scope or credential binding,
//!     unpinned host) are refused;
//! 12. another account's credential, which sees an empty collection and 404s on
//!     every id, never yields "confirmed gone"; and a record never bound to a
//!     credential cannot vouch for absence.
//!
//! Adding an adapter: implement [`Fixture`] over a mock backend that honours
//! the [`Fault`]s, and call [`run`] from a test. Catalog-only adapters (no
//! apply-time backend yet) have no fixture; the in-memory simulators run the
//! subset that applies to them.

#![allow(
    clippy::match_wildcard_for_single_variants,
    clippy::single_match_else,
    clippy::too_many_lines
)]

use std::sync::{Arc, Mutex};

use amicu_core::adapter::{Adapter, AdapterError, AdapterId, DescribeCapabilities, HealthStatus};
use amicu_core::diagnostic::{Category as DiagCategory, DiagnosticCode};
use amicu_core::secret::{SecretRef, SecretResolver, SecretValue};
use async_trait::async_trait;

use crate::capability::ProviderCapabilities;
use crate::provider::{
    Provider, ProvisionRequest, ProvisioningHandle, ProvisioningState, ResourceIdentity,
    TeardownOutcome,
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
    /// Every request is answered as if from a different zone/project/account
    /// (404s, or a host presenting another key), while the resource still
    /// exists.
    WrongScope,
    /// The backend answers as another account would: a 200 but empty
    /// collection, and 404 for every id.
    OtherAccount,
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
    /// Whether the backend can vouch that a never-created resource is absent.
    /// In-memory simulators cannot (their memory may simply be lost).
    fn absent_is_gone(&self) -> bool {
        true
    }
    /// Whether the adapter binds records to the credential that created them.
    fn binds_credentials(&self) -> bool {
        false
    }
    /// Whether prepared records carry a scope fingerprint.
    fn has_scope(&self) -> bool {
        true
    }
    /// Insert a foreign resource directly into the backend. `false` if the
    /// fixture cannot.
    fn seed(&self, _name: &str, _offering: &str, _region: &str) -> bool {
        false
    }
    /// Whether the backend lets the adapter see two resources with one name.
    fn detects_duplicates(&self) -> bool {
        true
    }
    /// Whether refusing a tampered record leaves the backend untouched. A host
    /// agent may legitimately stop the resource and merely be unable to confirm.
    fn refusal_is_inert(&self) -> bool {
        true
    }
    /// Tampered copies of a good record that teardown must refuse.
    fn bad_records(&self, _good: &ProvisioningHandle) -> Vec<ProvisioningHandle> {
        Vec::new()
    }
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

/// Resolves the same reference to a different token: another account.
struct OtherResolver;

#[async_trait]
impl SecretResolver for OtherResolver {
    async fn resolve(&self, _secret: &SecretRef) -> Result<SecretValue, AdapterError> {
        Ok(SecretValue::new("another-accounts-token".to_string()))
    }
}

/// Prepare a record and bind it to the credential, as `apply` does before it
/// persists the record.
async fn bound<F: Fixture>(f: &F, p: &F::P) -> ProvisioningHandle {
    let mut record = p.prepare(&req(f), identity()).unwrap();
    let secret = Resolver.resolve(&SecretRef::new("cred")).await.unwrap();
    record.bind_credential(&secret).unwrap();
    record
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
    failure_after_create_is_stoppable_and_retry_recovers(&make()).await;
    foreign_resources_are_untouched(&make()).await;
    adoption_refuses_mismatch_and_ambiguity(&make()).await;
    wrong_scope_is_not_gone(&make()).await;
    tampered_records_are_refused(&make()).await;
    other_credential_never_confirms(&make()).await;
    unbound_record_cannot_vouch_for_absence(&make()).await;
}

fn req<F: Fixture>(f: &F) -> ProvisionRequest {
    f.request().with_credentials(SecretRef::new("cred"))
}

fn not_confirmed(outcome: &Result<TeardownOutcome, AdapterError>) -> bool {
    !matches!(outcome, Ok(TeardownOutcome::ConfirmedGone))
}

async fn prepare_creates_nothing_and_record_is_self_describing<F: Fixture>(f: &F) {
    let p = f.provider();
    let prepared = p.prepare(&req(f), identity()).expect("prepare");
    // Write-ahead: the record exists while the backend holds nothing.
    assert_eq!(f.live_resources(), 0, "prepare must not create resources");
    assert!(!prepared.has_resource_id());
    assert_eq!(prepared.identity.deployment_id, "deployment-1");
    assert_eq!(prepared.identity.project_id, "project-1");
    assert_eq!(prepared.identity.nonce.len(), 32, "nonce set at prepare");
    assert!(prepared.identity.name.ends_with(&prepared.identity.nonce));
    assert_eq!(prepared.provider, p.capabilities().provider);
    assert_eq!(prepared.credentials, Some(SecretRef::new("cred")));
    assert_eq!(prepared.scope.is_some(), f.has_scope());
    assert_eq!(round_trip(&prepared), prepared);
    // Two records for the same deployment never share a name.
    let other = bound(f, &p).await;
    assert_ne!(prepared.identity.name, other.identity.name);

    let done = p
        .provision(prepared.clone(), &Resolver)
        .await
        .expect("provision");
    assert_eq!(done.credential_binding.is_some(), f.binds_credentials());
    assert_eq!(f.live_resources(), 1);
    assert!(done.has_resource_id(), "provision records the resource id");
    assert_eq!(done.identity, prepared.identity);
    assert_eq!(round_trip(&done), done);
}

async fn teardown_from_record_only<F: Fixture>(f: &F) {
    let prepared = bound(f, &f.provider()).await;
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
    // Never created.
    let prepared = bound(f, &p).await;
    let outcome = p.teardown(&prepared, &Resolver).await.unwrap();
    if f.absent_is_gone() {
        assert_eq!(outcome, TeardownOutcome::ConfirmedGone);
    } else {
        assert!(
            matches!(outcome, TeardownOutcome::Unconfirmed { .. }),
            "a backend that cannot vouch for absence must not confirm"
        );
    }
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
    let prepared = bound(f, &p).await;
    let done = p.provision(prepared, &Resolver).await.unwrap();

    if f.supports(Fault::DeleteLingers) {
        f.set_fault(Fault::DeleteLingers, true);
        let outcome = p.teardown(&done, &Resolver).await;
        assert!(
            not_confirmed(&outcome),
            "a lingering resource must not be confirmed gone"
        );
        assert_eq!(f.live_resources(), 1);
        f.set_fault(Fault::DeleteLingers, false);
    }
    if f.supports(Fault::DeleteFails) {
        f.set_fault(Fault::DeleteFails, true);
        let outcome = p.teardown(&done, &Resolver).await;
        assert!(
            not_confirmed(&outcome),
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
    if !f.supports(Fault::CreateResponseLost) {
        return;
    }
    let p = f.provider();
    let prepared = bound(f, &p).await;
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
    let prepared = bound(f, &p).await;
    if f.supports(Fault::CreateResponseLost) {
        f.set_fault(Fault::CreateResponseLost, true);
        assert!(p.provision(prepared.clone(), &Resolver).await.is_err());
    }
    // After a crash: the retry adopts the (possibly stopped) resource and
    // leaves it running. Without a crash: a second provision is a no-op.
    let first = p.provision(prepared.clone(), &Resolver).await.unwrap();
    let retried = p.provision(prepared, &Resolver).await.unwrap();
    assert_eq!(first.id, retried.id);
    assert_eq!(f.live_resources(), 1, "retry must adopt, not duplicate");
    assert!(
        p.status(&retried, &Resolver).await.unwrap().is_ready(),
        "an adopted resource must end up running"
    );
    assert_eq!(
        p.teardown(&retried, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 0);
}

async fn failure_after_create_is_stoppable_and_retry_recovers<F: Fixture>(f: &F) {
    if !f.supports(Fault::PostCreateStepFails) {
        return;
    }
    let p = f.provider();
    // (a) Stop after the failure.
    let prepared = bound(f, &p).await;
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

    // (b) Retry after the failure: the stopped server is adopted and booted.
    let prepared = bound(f, &p).await;
    assert!(p.provision(prepared.clone(), &Resolver).await.is_err());
    f.set_fault(Fault::PostCreateStepFails, false);
    let done = p.provision(prepared, &Resolver).await.unwrap();
    assert_eq!(f.live_resources(), 1);
    assert!(p.status(&done, &Resolver).await.unwrap().is_ready());
    assert_eq!(
        p.teardown(&done, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
}

async fn foreign_resources_are_untouched<F: Fixture>(f: &F) {
    let p = f.provider();
    let request = f.request();
    let ours = bound(f, &p).await;
    let sibling = bound(f, &p).await;
    if !f.seed(
        &sibling.identity.name,
        &request.offering_id,
        &request.region,
    ) {
        return;
    }
    // Many foreign resources, more than one listing page.
    for i in 0..60 {
        assert!(f.seed(&format!("other-{i}"), &request.offering_id, &request.region));
    }
    let done = p.provision(ours, &Resolver).await.unwrap();
    assert_ne!(
        done.identity.name, sibling.identity.name,
        "never adopt another identity's resource"
    );
    assert_eq!(
        f.live_resources(),
        62,
        "ours was created next to the foreign"
    );
    assert_eq!(
        f.bare_provider().teardown(&done, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 61, "foreign resources must survive");
}

async fn adoption_refuses_mismatch_and_ambiguity<F: Fixture>(f: &F) {
    let p = f.provider();
    let request = f.request();
    let prepared = bound(f, &p).await;
    if !f.seed(&prepared.identity.name, "other-offering", &request.region) {
        return;
    }
    assert!(
        p.provision(prepared.clone(), &Resolver).await.is_err(),
        "a resource with another offering must not be adopted"
    );
    assert_eq!(
        f.live_resources(),
        1,
        "and must not be deleted by provision"
    );
    if f.detects_duplicates() {
        assert!(f.seed(
            &prepared.identity.name,
            &request.offering_id,
            &request.region
        ));
        assert!(
            p.provision(prepared.clone(), &Resolver).await.is_err(),
            "several matches must be an error, not a guess"
        );
        assert_eq!(f.live_resources(), 2);
    }
    assert_eq!(
        p.teardown(&prepared, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 0);
}

async fn wrong_scope_is_not_gone<F: Fixture>(f: &F) {
    if !f.supports(Fault::WrongScope) {
        return;
    }
    let p = f.provider();
    let prepared = bound(f, &p).await;
    let done = p.provision(prepared, &Resolver).await.unwrap();
    f.set_fault(Fault::WrongScope, true);
    let outcome = f.bare_provider().teardown(&done, &Resolver).await;
    assert!(
        not_confirmed(&outcome),
        "a 404 at the wrong scope must never be confirmation"
    );
    assert_eq!(f.live_resources(), 1);
    f.set_fault(Fault::WrongScope, false);
    assert_eq!(
        p.teardown(&done, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
}

async fn other_credential_never_confirms<F: Fixture>(f: &F) {
    if !f.supports(Fault::OtherAccount) {
        return;
    }
    let p = f.provider();
    let prepared = bound(f, &p).await;
    // A bound record refuses to provision under another credential.
    assert!(p.provision(prepared.clone(), &OtherResolver).await.is_err());
    assert_eq!(f.live_resources(), 0);

    let mut done = p.provision(prepared, &Resolver).await.unwrap();
    f.set_fault(Fault::OtherAccount, true);
    let outcome = f.bare_provider().teardown(&done, &OtherResolver).await;
    assert!(
        matches!(outcome, Ok(TeardownOutcome::Unconfirmed { .. })),
        "another account's empty view must read as unconfirmed, got {outcome:?}"
    );
    assert_eq!(f.live_resources(), 1);
    f.set_fault(Fault::OtherAccount, false);

    // Rebinding is an explicit act; once done, the new credential is trusted.
    let other = OtherResolver
        .resolve(&SecretRef::new("cred"))
        .await
        .unwrap();
    done.rebind_credential(&other);
    assert_eq!(
        p.teardown(&done, &OtherResolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
    );
    assert_eq!(f.live_resources(), 0);
}

async fn unbound_record_cannot_vouch_for_absence<F: Fixture>(f: &F) {
    if !f.binds_credentials() {
        return;
    }
    let p = f.provider();
    // Nothing exists and the record was never bound: an empty listing may be
    // another account's view, so this is unconfirmed, not gone.
    let raw = p.prepare(&req(f), identity()).unwrap();
    assert!(raw.credential_binding.is_none());
    let outcome = p.teardown(&raw, &Resolver).await.unwrap();
    assert!(matches!(outcome, TeardownOutcome::Unconfirmed { .. }));
    // But a resource positively found under the identity can still be stopped
    // and confirmed (crash before the binding was persisted).
    if f.supports(Fault::CreateResponseLost) {
        f.set_fault(Fault::CreateResponseLost, true);
        assert!(p.provision(raw.clone(), &Resolver).await.is_err());
        assert_eq!(f.live_resources(), 1);
        assert_eq!(
            p.teardown(&raw, &Resolver).await.unwrap(),
            TeardownOutcome::ConfirmedGone
        );
        assert_eq!(f.live_resources(), 0);
    }
}

async fn tampered_records_are_refused<F: Fixture>(f: &F) {
    let p = f.provider();
    let prepared = bound(f, &p).await;
    let done = p.provision(prepared, &Resolver).await.unwrap();
    for bad in f.bad_records(&done) {
        let outcome = f.bare_provider().teardown(&bad, &Resolver).await;
        assert!(not_confirmed(&outcome), "tampered record accepted: {bad:?}");
        if f.refusal_is_inert() {
            assert_eq!(f.live_resources(), 1, "tampered record changed the backend");
        }
    }
    assert_eq!(
        p.teardown(&done, &Resolver).await.unwrap(),
        TeardownOutcome::ConfirmedGone
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

#[derive(Clone)]
struct Res {
    id: String,
    name: String,
    offering: String,
    region: String,
    /// Provider-native state string.
    state: String,
}

const MOCK_PROJECT_ID: &str = "proj-1";
const MOCK_ORG_ID: &str = "org-1";

#[derive(Default)]
struct Store {
    resources: Vec<Res>,
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

const MOCK_ZONE: &str = "fr-par-1";
const MOCK_PROJECT: &str = "amicu-project";

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

    /// The provider-native "running" state.
    fn running(self) -> &'static str {
        match self {
            Dialect::Hetzner | Dialect::Scaleway => "running",
            Dialect::RunPod => "RUNNING",
            Dialect::Ovh => "ACTIVE",
        }
    }

    /// The provider-native state of a freshly created resource.
    fn initial(self) -> &'static str {
        match self {
            Dialect::Scaleway => "stopped",
            other => other.running(),
        }
    }

    fn object(self, r: &Res) -> serde_json::Value {
        use serde_json::json;
        match self {
            Dialect::Hetzner => json!({
                "id": r.id.parse::<i64>().unwrap(), "name": r.name, "status": r.state,
                "server_type": {"name": r.offering},
                "datacenter": {"location": {"name": r.region}},
            }),
            Dialect::RunPod => json!({
                "id": r.id, "name": r.name, "desiredStatus": r.state,
                "machine": {"gpuTypeId": r.offering, "dataCenterId": r.region},
            }),
            Dialect::Scaleway => json!({
                "id": r.id, "name": r.name, "state": r.state, "commercial_type": r.offering,
                "project": MOCK_PROJECT_ID, "organization": MOCK_ORG_ID,
            }),
            Dialect::Ovh => json!({
                "id": r.id, "name": r.name, "status": r.state,
                "flavorId": r.offering, "region": r.region,
            }),
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

    /// `(offering, region)` as the create body carries them.
    fn create_fields(self, body: &serde_json::Value) -> (String, String) {
        let s =
            |v: Option<&serde_json::Value>| v.and_then(|v| v.as_str()).unwrap_or("").to_string();
        match self {
            Dialect::Hetzner => (s(body.get("server_type")), s(body.get("location"))),
            Dialect::RunPod => (
                s(body.pointer("/gpuTypeIds/0")),
                s(body.pointer("/dataCenterIds/0")),
            ),
            Dialect::Scaleway => (s(body.get("commercial_type")), String::new()),
            Dialect::Ovh => (s(body.get("flavorId")), s(body.get("region"))),
        }
    }
}

/// A stateful mock cloud REST API. It ignores server-side name filters (the
/// adapter must filter client-side) and pages like the real Hetzner/Scaleway
/// APIs.
struct HttpCloud {
    dialect: Dialect,
    base: &'static str,
    store: Arc<Mutex<Store>>,
}

fn json_response(status: u16, value: &serde_json::Value) -> crate::transport::HttpResponse {
    crate::transport::HttpResponse::new(status, serde_json::to_vec(value).unwrap())
}

fn query_param<'a>(query: &'a str, key: &str) -> Option<&'a str> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
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
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let coll = self.dialect.collection();
        let d = self.dialect;
        let mut st = self.store.lock().unwrap();
        let not_found = || Ok(HttpResponse::new(404, Vec::new()));

        // Scope: a request addressed to the wrong zone/project, or any request
        // while `WrongScope` is armed, is answered as "unknown" (404).
        if st.faults.has(Fault::WrongScope) {
            return not_found();
        }
        match d {
            Dialect::Scaleway if segs.get(1) != Some(&MOCK_ZONE) => return not_found(),
            Dialect::Ovh if segs.get(2) != Some(&MOCK_PROJECT) => return not_found(),
            _ => {}
        }
        // Another account's view: an empty collection (200) and unknown ids (404).
        if st.faults.has(Fault::OtherAccount) && !(d == Dialect::Ovh && segs.len() == 3) {
            if request.method == HttpMethod::Get && segs.last() == Some(&coll) {
                return Ok(json_response(200, &d.list(Vec::new())));
            }
            if segs.len() >= 2 && segs[segs.len() - 2] == coll {
                return not_found();
            }
        }
        // OVH project probe.
        if d == Dialect::Ovh && segs.len() == 3 {
            return Ok(json_response(
                200,
                &serde_json::json!({ "project_id": "proj-1" }),
            ));
        }

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

        // POST .../servers/{id}/action (power on)
        if request.method == HttpMethod::Post && last == "action" && prev2 == coll {
            if st.faults.has(Fault::PostCreateStepFails) {
                return Ok(HttpResponse::new(500, Vec::new()));
            }
            let running = d.running();
            if let Some(r) = st.resources.iter_mut().find(|r| r.id == prev) {
                r.state = running.to_string();
            }
            return Ok(json_response(202, &serde_json::json!({})));
        }
        // collection
        if last == coll {
            return match request.method {
                HttpMethod::Post => {
                    let body = request.body.clone().expect("create body");
                    let name = body
                        .get("name")
                        .and_then(|n| n.as_str())
                        .expect("create body carries the identity name")
                        .to_string();
                    let (offering, region) = d.create_fields(&body);
                    st.next += 1;
                    let res = Res {
                        id: d.new_id(st.next),
                        name,
                        offering,
                        region,
                        state: d.initial().to_string(),
                    };
                    st.resources.push(res.clone());
                    if st.faults.has(Fault::CreateResponseLost) {
                        st.faults.set(Fault::CreateResponseLost, false);
                        return Err(TransportError::Timeout);
                    }
                    Ok(json_response(201, &d.one(d.object(&res))))
                }
                HttpMethod::Get => {
                    let per_page: usize = query_param(query, "per_page")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(25);
                    let page: usize = query_param(query, "page")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(1);
                    let paged = matches!(d, Dialect::Hetzner | Dialect::Scaleway);
                    let objs = st
                        .resources
                        .iter()
                        .skip(if paged { (page - 1) * per_page } else { 0 })
                        .take(if paged { per_page } else { usize::MAX })
                        .map(|r| d.object(r))
                        .collect();
                    Ok(json_response(200, &d.list(objs)))
                }
                _ => Ok(HttpResponse::new(405, Vec::new())),
            };
        }
        // item
        if prev == coll {
            let found = st.resources.iter().position(|r| r.id == last);
            return match (request.method, found) {
                (HttpMethod::Get, Some(ix)) => {
                    let r = st.resources[ix].clone();
                    Ok(json_response(200, &d.one(d.object(&r))))
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
                (_, None) => not_found(),
                _ => Ok(HttpResponse::new(405, Vec::new())),
            };
        }
        not_found()
    }
}

const HOST_KEY: &str = "SHA256:conformance-host-key";

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
        // A different machine answers: another host key, and it knows nothing.
        let wrong = st.faults.has(Fault::WrongScope);
        let key = if wrong {
            "SHA256:another-host"
        } else {
            HOST_KEY
        };
        let out = |status: i32, s: &str, e: &str| {
            Ok(
                CommandOutput::new(status, s.as_bytes().to_vec(), e.as_bytes().to_vec())
                    .with_host_key(key),
            )
        };
        if wrong {
            return out(44, "", "");
        }
        let flag = |name: &str| {
            words
                .iter()
                .position(|w| w == name)
                .map(|i| words[i + 1].clone())
        };
        if verb == "provision" {
            let name = flag("--name").expect("--name");
            let offering = flag("--offering").expect("--offering");
            let region = flag("--region").expect("--region");
            let id = match st.resources.iter().find(|r| r.name == name) {
                Some(r) if r.offering != offering || r.region != region => {
                    return out(3, "", "conflict");
                }
                Some(r) => r.id.clone(),
                None => {
                    st.next += 1;
                    let id = format!("agent-{}", st.next);
                    st.resources.push(Res {
                        id: id.clone(),
                        name,
                        offering,
                        region,
                        state: "ready".into(),
                    });
                    id
                }
            };
            if st.faults.has(Fault::CreateResponseLost) {
                st.faults.set(Fault::CreateResponseLost, false);
                return Err(SshTransportError::Timeout);
            }
            return out(0, &id, "");
        }
        let target_key = if by_name { &words[3] } else { &words[2] };
        let found = st.resources.iter().position(|r| {
            if by_name {
                r.name == *target_key
            } else {
                r.id == *target_key
            }
        });
        match (verb, found) {
            ("status", Some(_)) => out(0, "ready", ""),
            ("teardown", _) if st.faults.has(Fault::DeleteFails) => out(1, "", "boom"),
            ("teardown", Some(ix)) => {
                if !st.faults.has(Fault::DeleteLingers) {
                    st.resources.remove(ix);
                }
                out(0, "", "")
            }
            (_, None) => out(44, "", ""),
            _ => out(2, "", "bad verb"),
        }
    }
}

fn seed_into(
    store: &Mutex<Store>,
    new_id: impl Fn(u32) -> String,
    dialect_running: &str,
    name: &str,
    offering: &str,
    region: &str,
) {
    let mut st = store.lock().unwrap();
    st.next += 1;
    let id = new_id(st.next);
    st.resources.push(Res {
        id,
        name: name.into(),
        offering: offering.into(),
        region: region.into(),
        state: dialect_running.into(),
    });
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

    fn seed(&self, name: &str, offering: &str, region: &str) -> bool {
        let d = self.dialect;
        seed_into(
            &self.store,
            |n| d.new_id(n),
            d.running(),
            name,
            offering,
            region,
        );
        true
    }

    fn binds_credentials(&self) -> bool {
        true
    }

    fn bad_records(&self, good: &ProvisioningHandle) -> Vec<ProvisioningHandle> {
        let mut out = Vec::new();
        let mut tamper = |f: &dyn Fn(&mut ProvisioningHandle)| {
            let mut r = good.clone();
            f(&mut r);
            out.push(r);
        };
        // URL-significant name and id.
        tamper(&|r| r.identity.name = "a/b?c".into());
        tamper(&|r| r.id = "1/../2".into());
        // Forged or missing scope fingerprint.
        tamper(&|r| r.scope = Some("forged".into()));
        tamper(&|r| r.scope = None);
        // A forged credential binding (another account's token).
        tamper(&|r| r.credential_binding = Some("amicu-cred-v1:forged".into()));
        match self.dialect {
            Dialect::Scaleway => {
                // A different zone, and no zone (no region fallback).
                tamper(&|r| r.zone = Some("fr-par-9".into()));
                tamper(&|r| r.zone = None);
            }
            Dialect::Ovh => {
                // A different project, and no project.
                tamper(&|r| {
                    r.resources.insert("project".into(), "other-project".into());
                });
                tamper(&|r| {
                    r.resources.remove("project");
                });
                // A different account behind the same project name.
                tamper(&|r| r.account_id = Some("someone-else".into()));
            }
            _ => {}
        }
        out
    }
}

struct SelfHostedFixture {
    store: Arc<Mutex<Store>>,
    /// Whether the operator declared the host key (pinned from `prepare`).
    declared_key: bool,
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
        let mut host = crate::self_hosted::DeclaredHost::new(
            "rack-de-01",
            Region::new("dc-frankfurt", Residency::Eu),
        );
        if self.declared_key {
            host = host.with_host_key(HOST_KEY);
        }
        crate::self_hosted::SelfHostedProvider::new()
            .with_host(host)
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
        !matches!(fault, Fault::PostCreateStepFails | Fault::OtherAccount)
    }

    fn seed(&self, name: &str, offering: &str, region: &str) -> bool {
        seed_into(
            &self.store,
            |n| format!("agent-{n}"),
            "ready",
            name,
            offering,
            region,
        );
        true
    }

    fn detects_duplicates(&self) -> bool {
        false
    }

    fn refusal_is_inert(&self) -> bool {
        false
    }

    fn bad_records(&self, good: &ProvisioningHandle) -> Vec<ProvisioningHandle> {
        // Never pinned: the host's word alone must not confirm. (A wrong pin is
        // covered by `WrongScope`.)
        let mut unpinned = good.clone();
        unpinned.ssh_host_key = None;
        // A record edited to point at another host no longer matches its scope.
        let mut redirected = good.clone();
        redirected
            .resources
            .insert("ssh_host".into(), "elsewhere.example".into());
        let mut unscoped = good.clone();
        unscoped.scope = None;
        vec![unpinned, redirected, unscoped]
    }
}

/// Shares one in-memory simulator between the "configured" and "bare"
/// providers, so teardown-from-record is exercised against the same state.
struct Shared<T>(Arc<T>);

#[async_trait]
impl<T: Provider + 'static> Adapter for Shared<T> {
    fn id(&self) -> AdapterId {
        self.0.id()
    }

    async fn health(&self) -> Result<HealthStatus, AdapterError> {
        self.0.health().await
    }
}

impl<T: Provider + 'static> DescribeCapabilities for Shared<T> {
    fn describe(&self) -> serde_json::Value {
        self.0.describe()
    }
}

#[async_trait]
impl<T: Provider + 'static> Provider for Shared<T> {
    fn capabilities(&self) -> ProviderCapabilities {
        self.0.capabilities()
    }

    fn prepare(
        &self,
        request: &ProvisionRequest,
        identity: ResourceIdentity,
    ) -> Result<ProvisioningHandle, AdapterError> {
        self.0.prepare(request, identity)
    }

    async fn provision(
        &self,
        prepared: ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningHandle, AdapterError> {
        self.0.provision(prepared, resolver).await
    }

    async fn status(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<ProvisioningState, AdapterError> {
        self.0.status(handle, resolver).await
    }

    async fn teardown(
        &self,
        handle: &ProvisioningHandle,
        resolver: &dyn SecretResolver,
    ) -> Result<TeardownOutcome, AdapterError> {
        self.0.teardown(handle, resolver).await
    }
}

/// An in-memory simulator (catalog-only adapter) as a fixture: no fault
/// injection, but the identity/idempotence/unconfirmed rules still apply.
struct SimFixture<T> {
    sim: Arc<T>,
    request: ProvisionRequest,
    live: fn(&T) -> usize,
}

impl<T: Provider + 'static> Fixture for SimFixture<T> {
    type P = Shared<T>;

    fn provider(&self) -> Shared<T> {
        Shared(self.sim.clone())
    }

    fn bare_provider(&self) -> Shared<T> {
        self.provider()
    }

    fn request(&self) -> ProvisionRequest {
        self.request.clone()
    }

    fn live_resources(&self) -> usize {
        (self.live)(&self.sim)
    }

    fn set_fault(&self, _fault: Fault, _on: bool) {}

    fn supports(&self, _fault: Fault) -> bool {
        false
    }

    fn absent_is_gone(&self) -> bool {
        false
    }

    fn has_scope(&self) -> bool {
        false
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

fn sim_fixture<T>(sim: T, offering: &str, region: &str, live: fn(&T) -> usize) -> SimFixture<T> {
    SimFixture {
        sim: Arc::new(sim),
        request: ProvisionRequest::new(offering, region),
        live,
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
                "gpu.a100.1x",
                "GRA",
            )
        })
        .await;
    }

    #[tokio::test]
    async fn self_hosted_conforms() {
        run(|| SelfHostedFixture {
            store: Arc::default(),
            declared_key: true,
        })
        .await;
    }

    #[tokio::test]
    async fn self_hosted_pins_host_key_on_first_use() {
        // No declared key: the key seen at provision is pinned in the record, and
        // teardown from that record is then confirmed.
        let f = SelfHostedFixture {
            store: Arc::default(),
            declared_key: false,
        };
        let p = f.provider();
        let prepared = p.prepare(&req(&f), identity()).unwrap();
        assert!(prepared.ssh_host_key.is_none());
        let done = p.provision(prepared, &Resolver).await.unwrap();
        assert_eq!(done.ssh_host_key.as_deref(), Some(HOST_KEY));
        assert_eq!(
            f.bare_provider().teardown(&done, &Resolver).await.unwrap(),
            TeardownOutcome::ConfirmedGone
        );
    }

    #[tokio::test]
    async fn self_hosted_without_a_pin_never_confirms() {
        // Crash before the first connection with no declared key: nothing pins
        // the host, so its word alone cannot confirm absence.
        let f = SelfHostedFixture {
            store: Arc::default(),
            declared_key: false,
        };
        let p = f.provider();
        let prepared = p.prepare(&req(&f), identity()).unwrap();
        let outcome = p.teardown(&prepared, &Resolver).await.unwrap();
        assert!(matches!(outcome, TeardownOutcome::Unconfirmed { .. }));
    }

    #[tokio::test]
    async fn self_hosted_legacy_record_falls_back_to_declared_host() {
        let f = SelfHostedFixture {
            store: Arc::default(),
            declared_key: true,
        };
        let p = f.provider();
        let prepared = p.prepare(&req(&f), identity()).unwrap();
        let mut legacy = p.provision(prepared, &Resolver).await.unwrap();
        for key in ["ssh_host", "ssh_user", "ssh_port"] {
            legacy.resources.remove(key);
        }
        legacy.ssh_host_key = None;
        legacy.scope = None;
        assert_eq!(
            p.teardown(&legacy, &Resolver).await.unwrap(),
            TeardownOutcome::ConfirmedGone
        );
        assert_eq!(f.live_resources(), 0);
    }

    #[tokio::test]
    async fn self_hosted_provision_rejects_a_redirected_target() {
        let f = SelfHostedFixture {
            store: Arc::default(),
            declared_key: true,
        };
        let p = f.provider();
        let mut prepared = p.prepare(&req(&f), identity()).unwrap();
        prepared
            .resources
            .insert("ssh_host".into(), "attacker.example".into());
        assert!(p.provision(prepared, &Resolver).await.is_err());
        assert_eq!(f.live_resources(), 0);
    }

    // The in-memory simulators run the subset of the contract that applies
    // (no backend to inject faults into or seed).

    #[tokio::test]
    async fn aws_sim_conforms() {
        run(|| {
            sim_fixture(crate::AwsProvider::new(), "g5.xlarge", "eu-north-1", |s| {
                s.live_instances().len()
            })
        })
        .await;
    }

    #[tokio::test]
    async fn azure_sim_conforms() {
        run(|| {
            sim_fixture(
                crate::AzureProvider::new(),
                "Standard_NC24ads_A100_v4",
                "westeurope",
                |s| s.live_instances().len(),
            )
        })
        .await;
    }

    #[tokio::test]
    async fn gcp_sim_conforms() {
        run(|| {
            sim_fixture(
                crate::GcpProvider::new(),
                "a2-ultragpu-1g",
                "europe-west4",
                |s| s.live_instances().len(),
            )
        })
        .await;
    }

    #[tokio::test]
    async fn nebius_sim_conforms() {
        run(|| {
            sim_fixture(
                crate::NebiusProvider::new(),
                "gpu.h100-80gb.1x",
                "eu-north1",
                |s| s.live_instances().len(),
            )
        })
        .await;
    }

    #[tokio::test]
    async fn fake_sim_conforms() {
        run(|| {
            sim_fixture(
                crate::FakeProvider::new("fake-cloud"),
                "gpu.h100.1x",
                "eu-central-1",
                |s| s.live_instances().len(),
            )
        })
        .await;
    }

    // Modal has no apply-time backend yet (every call is Unsupported), so it has
    // no fixture; wiring it to `apply` requires adding one here first.
}
