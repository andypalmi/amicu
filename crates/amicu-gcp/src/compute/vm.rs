//! The VM lifecycle: write-ahead [`prepare`], [`create`] with recovery from a
//! lost response, [`get`], [`status`] and [`delete`].
//!
//! The stop record ([`VmRecord`]) is what a caller persists before anything is
//! sent: [`prepare`] is pure, so the record exists before [`create`] sends the
//! insert. Everything that changes or deletes a VM first checks the owner
//! marker in the record against the VM's labels.

use std::collections::BTreeMap;

use amicu_core::naming::project_app_name;
use amicu_providers::HttpMethod;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::spec::VmSpec;
use crate::client::{COMPUTE_BASE_URL, GcpClient};
use crate::error::GcpError;
use crate::labels::ensure_owned;
use crate::operations::{PollTiming, wait_compute_zone_operation};
use crate::validate::{validate_project_id, validate_resource_name, validate_zone};

/// Tries of `instances.insert` when the response is lost and the VM turns out
/// not to exist.
const INSERT_ATTEMPTS: u32 = 3;

/// Everything needed to find and stop the VM, free of secrets. Persist it
/// before [`create`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRecord {
    pub project: String,
    pub zone: String,
    pub name: String,
    /// The owner marker the VM carries in its [`crate::OWNER_LABEL`] label.
    pub owner: String,
}

/// A VM as Compute Engine reports it (the fields amicu reads; instance
/// metadata, which holds the secrets, is deliberately not kept).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Instance {
    pub name: String,
    pub status: String,
    pub status_message: Option<String>,
    pub last_stop_timestamp: Option<String>,
    pub labels: Option<BTreeMap<String, String>>,
    scheduling: SchedulingView,
    network_interfaces: Vec<NicView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct SchedulingView {
    provisioning_model: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct NicView {
    access_configs: Vec<AccessConfigView>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AccessConfigView {
    #[serde(rename = "natIP")]
    nat_ip: Option<String>,
}

impl Instance {
    /// The external IPv4 address, once assigned.
    #[must_use]
    pub fn external_ip(&self) -> Option<&str> {
        self.network_interfaces
            .iter()
            .flat_map(|n| &n.access_configs)
            .find_map(|a| a.nat_ip.as_deref())
            .filter(|ip| !ip.is_empty())
    }

    /// The state amicu acts on.
    #[must_use]
    pub fn state(&self) -> VmState {
        match self.status.as_str() {
            "PROVISIONING" => VmState::Provisioning,
            "STAGING" => VmState::Staging,
            "RUNNING" => VmState::Running {
                external_ip: self.external_ip().map(str::to_owned),
            },
            "STOPPING" => VmState::Stopping,
            "SUSPENDED" => VmState::Suspended,
            // A stopped VM reads `TERMINATED` in the REST API.
            "TERMINATED" | "STOPPED" => VmState::Terminated {
                preempted: self.was_preempted(),
            },
            other => VmState::Other(other.to_owned()),
        }
    }

    /// A Spot VM Compute Engine stopped: the status message says so, or a Spot
    /// VM that is stopped has a `lastStopTimestamp` (only an action Compute
    /// Engine took leaves a Spot VM there, since amicu never stops one).
    fn was_preempted(&self) -> bool {
        let said_so = self
            .status_message
            .as_deref()
            .is_some_and(|m| m.to_ascii_lowercase().contains("preempt"));
        let spot = self.scheduling.provisioning_model.as_deref() == Some("SPOT");
        said_so
            || (spot
                && self
                    .last_stop_timestamp
                    .as_deref()
                    .is_some_and(|t| !t.is_empty()))
    }
}

/// Where the VM is in its life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmState {
    Provisioning,
    Staging,
    Running {
        external_ip: Option<String>,
    },
    Stopping,
    /// Stopped. `preempted` is a Spot reclaim (with the DELETE termination
    /// action amicu sets, a preempted VM usually disappears: [`Gone`](Self::Gone)).
    Terminated {
        preempted: bool,
    },
    Suspended,
    /// The VM does not exist (deleted by the run-duration backstop, by a
    /// preemption with the DELETE action, or by someone else).
    Gone,
    Other(String),
}

/// What [`delete`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// The VM was already absent.
    AlreadyGone,
    /// Deleted, and a `get` confirmed it.
    Deleted,
}

/// A unique instance name: the shared `project_app_name` scheme plus the
/// deployment's `nonce` (lowercase letters and digits, generated once and kept
/// in the record), cut to Compute Engine's 63 characters.
///
/// # Errors
/// [`GcpError::Invalid`] for a bad nonce.
pub fn instance_name(
    project_id: &str,
    deployment_id: &str,
    nonce: &str,
) -> Result<String, GcpError> {
    if !(4..=12).contains(&nonce.len())
        || !nonce
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    {
        return Err(GcpError::Invalid {
            kind: "name nonce",
            value: nonce.to_owned(),
            reason: "expected 4 to 12 lowercase letters or digits",
        });
    }
    let mut base = project_app_name(project_id, deployment_id);
    base.truncate(63 - 1 - nonce.len());
    while base.ends_with('-') {
        base.pop();
    }
    Ok(format!("{base}-{nonce}"))
}

/// The record for `spec`, before any request: no backend contact.
///
/// # Errors
/// [`GcpError::Invalid`] when the spec does not validate.
pub fn prepare(spec: &VmSpec) -> Result<VmRecord, GcpError> {
    spec.validate()?;
    Ok(VmRecord {
        project: spec.project.clone(),
        zone: spec.zone.clone(),
        name: spec.name.clone(),
        owner: spec.owner()?.to_owned(),
    })
}

/// Creates the VM and waits for the insert operation. `record` comes from
/// [`prepare`] and must have been persisted by the caller first.
///
/// A response lost to a timeout or a `5xx` is not retried blindly: the VM is
/// looked up by name first and adopted only when it carries the owner marker
/// (a `409` or an existing VM without it is [`GcpError::Conflict`], never
/// adopted). Every failure after the first request leaves a stoppable record:
/// call [`delete`] with it.
///
/// # Errors
/// [`GcpError::ZoneExhausted`] when the zone has no capacity (try another
/// zone), [`GcpError::QuotaExceeded`], [`GcpError::Conflict`], or any API error.
pub async fn create(
    client: &GcpClient,
    spec: &VmSpec,
    record: &VmRecord,
    timing: PollTiming,
) -> Result<Instance, GcpError> {
    if *record != prepare(spec)? {
        return Err(GcpError::Invalid {
            kind: "VM record",
            value: record.name.clone(),
            reason: "does not match the VM spec",
        });
    }
    let body = spec.insert_body()?;
    let url = instances_url(record);
    let mut attempt = 0;
    loop {
        attempt += 1;
        match client
            .request(HttpMethod::Post, &url, Some(body.clone()))
            .await
        {
            Ok(op) => return finish_insert(client, record, &op, timing).await,
            Err(GcpError::Conflict { message }) => {
                return adopt(client, record, GcpError::Conflict { message }).await;
            }
            Err(e) => {
                let e = classify(e, &record.zone);
                let lost = matches!(e, GcpError::Transport(_))
                    || matches!(e, GcpError::Api { status, .. } if status >= 500);
                if !lost {
                    return Err(e);
                }
                // The insert may have been applied: look before retrying.
                match get_owned(client, record).await {
                    Ok(inst) => return Ok(inst),
                    Err(GcpError::NotFound { .. }) if attempt < INSERT_ATTEMPTS => {
                        tokio::time::sleep(timing.interval).await;
                    }
                    Err(GcpError::NotFound { .. }) => return Err(e),
                    Err(lookup) => return Err(lookup),
                }
            }
        }
    }
}

async fn finish_insert(
    client: &GcpClient,
    record: &VmRecord,
    op: &Value,
    timing: PollTiming,
) -> Result<Instance, GcpError> {
    let operation = op["name"]
        .as_str()
        .ok_or_else(|| GcpError::Decode("insert returned no operation name".into()))?;
    wait_compute_zone_operation(client, &record.project, &record.zone, operation, timing)
        .await
        .map_err(|e| classify(e, &record.zone))?;
    get_owned(client, record).await
}

/// A `409` on the name: ours from an earlier try (adopt) or somebody else's
/// (an error, untouched).
async fn adopt(
    client: &GcpClient,
    record: &VmRecord,
    conflict: GcpError,
) -> Result<Instance, GcpError> {
    match get_owned(client, record).await {
        Ok(inst) => Ok(inst),
        Err(GcpError::NotOwned { .. }) => Err(GcpError::Conflict {
            message: format!(
                "an instance named `{}` already exists without amicu's owner label; \
                 it is left alone",
                record.name
            ),
        }),
        Err(GcpError::NotFound { .. }) => Err(conflict),
        Err(e) => Err(e),
    }
}

/// Maps capacity and quota failures, which Google reports as an operation
/// error code or as an API error message, onto their own errors.
fn classify(e: GcpError, zone: &str) -> GcpError {
    let stockout = |text: &str| {
        text.contains("ZONE_RESOURCE_POOL_EXHAUSTED")
            || text.contains("does not have enough resources")
    };
    match e {
        GcpError::OperationFailed { code, message, .. }
            if stockout(&code) || stockout(&message) =>
        {
            GcpError::ZoneExhausted {
                zone: zone.to_owned(),
                message,
            }
        }
        GcpError::OperationFailed { code, message, .. } if code.contains("QUOTA") => {
            GcpError::QuotaExceeded { message }
        }
        GcpError::Api { message, .. } | GcpError::Conflict { message } if stockout(&message) => {
            GcpError::ZoneExhausted {
                zone: zone.to_owned(),
                message,
            }
        }
        other => other,
    }
}

/// The VM, whoever owns it.
///
/// # Errors
/// [`GcpError::NotFound`] when it does not exist.
pub async fn get(client: &GcpClient, record: &VmRecord) -> Result<Instance, GcpError> {
    check_record(record)?;
    client
        .request_as(HttpMethod::Get, &instance_url(record), None)
        .await
}

/// [`get`], refusing a VM without the record's owner marker.
async fn get_owned(client: &GcpClient, record: &VmRecord) -> Result<Instance, GcpError> {
    let inst = get(client, record).await?;
    ensure_owned(
        &format!("instance {}", record.name),
        inst.labels.as_ref(),
        &record.owner,
    )?;
    Ok(inst)
}

/// The VM's state; [`VmState::Gone`] when it does not exist.
///
/// # Errors
/// [`GcpError::NotOwned`] when a VM of that name is not ours, or an API error.
pub async fn status(client: &GcpClient, record: &VmRecord) -> Result<VmState, GcpError> {
    match get_owned(client, record).await {
        Ok(inst) => Ok(inst.state()),
        Err(GcpError::NotFound { .. }) => Ok(VmState::Gone),
        Err(e) => Err(e),
    }
}

/// Deletes the VM when it carries the record's owner marker, waits for the
/// operation and confirms with a `get` that ends in `404`.
///
/// # Errors
/// [`GcpError::NotOwned`] (nothing is deleted) for a VM of that name without
/// the marker, [`GcpError::OperationTimeout`] when it is still listed at the
/// deadline, or an API error.
pub async fn delete(
    client: &GcpClient,
    record: &VmRecord,
    timing: PollTiming,
) -> Result<DeleteOutcome, GcpError> {
    match get_owned(client, record).await {
        Ok(_) => {}
        Err(GcpError::NotFound { .. }) => return Ok(DeleteOutcome::AlreadyGone),
        Err(e) => return Err(e),
    }
    match client
        .request(HttpMethod::Delete, &instance_url(record), None)
        .await
    {
        Ok(op) => {
            if let Some(name) = op["name"].as_str() {
                match wait_compute_zone_operation(
                    client,
                    &record.project,
                    &record.zone,
                    name,
                    timing,
                )
                .await
                {
                    Ok(_) => {}
                    Err(e) if e.is_not_found() => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Err(e) if e.is_not_found() => {}
        Err(e) => return Err(e),
    }
    let deadline = tokio::time::Instant::now() + timing.timeout;
    loop {
        match get(client, record).await {
            Err(GcpError::NotFound { .. }) => return Ok(DeleteOutcome::Deleted),
            Ok(_) => {}
            Err(e) if e.is_retryable() => {}
            Err(e) => return Err(e),
        }
        if tokio::time::Instant::now() + timing.interval >= deadline {
            return Err(GcpError::OperationTimeout {
                operation: format!("delete {}", record.name),
            });
        }
        tokio::time::sleep(timing.interval).await;
    }
}

fn check_record(record: &VmRecord) -> Result<(), GcpError> {
    validate_project_id(&record.project)?;
    validate_zone(&record.zone)?;
    validate_resource_name(&record.name)
}

fn instances_url(record: &VmRecord) -> String {
    format!(
        "{COMPUTE_BASE_URL}/projects/{}/zones/{}/instances",
        record.project, record.zone
    )
}

fn instance_url(record: &VmRecord) -> String {
    format!("{}/{}", instances_url(record), record.name)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::time::Duration;

    use amicu_providers::{HttpResponse, TransportError};

    use super::*;
    use crate::client::tests::{Mock, client};
    use crate::compute::Provisioning;
    use crate::compute::spec::tests::spec;
    use crate::labels::{OWNER_LABEL, owner_labels, owner_marker};

    pub(crate) const INSTANCES: &str = "/projects/my-proj/zones/us-central1-a/instances";
    pub(crate) const ONE: &str =
        "/projects/my-proj/zones/us-central1-a/instances/amicu-demo-chat-0123456789ab-n1";
    const OP_WAIT: &str = "/projects/my-proj/zones/us-central1-a/operations/op-1/wait";
    const DONE: &str = r#"{"name":"op-1","status":"DONE"}"#;
    const PENDING_OP: &str = r#"{"name":"op-1","status":"RUNNING"}"#;

    pub(crate) fn fast() -> PollTiming {
        PollTiming {
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(30),
        }
    }

    fn owner() -> String {
        owner_marker(None, "demo", "chat")
    }

    pub(crate) fn instance_json(status: &str, owned: bool, extra: &str) -> String {
        let labels = if owned {
            serde_json::to_string(&owner_labels(None, "demo", "chat")).unwrap()
        } else {
            r#"{"env":"prod"}"#.to_owned()
        };
        format!(
            r#"{{"name":"amicu-demo-chat-0123456789ab-n1","status":"{status}","labels":{labels},
            "networkInterfaces":[{{"accessConfigs":[{{"natIP":"203.0.113.7"}}]}}]{extra}}}"#
        )
    }

    fn the_spec() -> VmSpec {
        spec(
            "n1-standard-4",
            Some(("nvidia-tesla-t4", 1)),
            Provisioning::Spot,
        )
    }

    fn posts(mock: &Mock) -> usize {
        mock.requests()
            .iter()
            .filter(|r| r.method == HttpMethod::Post && r.url.ends_with("/instances"))
            .count()
    }

    #[test]
    fn instance_names_are_unique_valid_and_bounded() {
        let n = instance_name("demo", "chat", "0123456789ab").unwrap();
        validate_resource_name(&n).unwrap();
        assert!(n.ends_with("-0123456789ab") && n.len() <= 63);
        let long = instance_name(&"p".repeat(80), &"d".repeat(80), "0123456789ab").unwrap();
        assert!(long.len() <= 63 && long.ends_with("-0123456789ab"));
        assert_ne!(n, instance_name("demo", "chat", "ba9876543210").unwrap());
        assert!(instance_name("demo", "chat", "A/..").is_err());
    }

    #[tokio::test]
    async fn prepare_returns_the_record_before_any_request() {
        let (_c, mock) = client(Mock::default());
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        assert_eq!(rec.owner, owner());
        assert_eq!(rec.name, s.name);
        assert!(mock.requests().is_empty());
        // The record round-trips through the caller's state.
        let again: VmRecord = serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(again, rec);
    }

    #[tokio::test]
    async fn create_sends_the_insert_first_then_waits_and_reads_back() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Post, INSTANCES, 200, PENDING_OP)
                .on(HttpMethod::Post, OP_WAIT, 200, DONE)
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("PROVISIONING", true, ""),
                ),
        );
        let inst = create(&c, &s, &rec, fast()).await.unwrap();
        assert_eq!(inst.state(), VmState::Provisioning);
        let seen = mock.requests();
        assert_eq!(seen[0].method, HttpMethod::Post);
        assert!(seen[0].url.ends_with("/zones/us-central1-a/instances"));
        assert_eq!(seen[0].body.as_ref().unwrap(), &s.insert_body().unwrap());
        assert!(seen[1].url.ends_with("/operations/op-1/wait"));
        assert_eq!(seen[2].method, HttpMethod::Get);
    }

    #[tokio::test]
    async fn create_rejects_a_record_for_another_vm() {
        let s = the_spec();
        let mut rec = prepare(&s).unwrap();
        rec.name = "other".into();
        let (c, mock) = client(Mock::default());
        assert!(matches!(
            create(&c, &s, &rec, fast()).await,
            Err(GcpError::Invalid { .. })
        ));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn lost_create_response_adopts_our_vm_without_a_second_insert() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let (c, mock) = client(
            Mock::default()
                .then(HttpMethod::Post, INSTANCES, Err(TransportError::Timeout))
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("STAGING", true, ""),
                ),
        );
        let inst = create(&c, &s, &rec, fast()).await.unwrap();
        assert_eq!(inst.state(), VmState::Staging);
        assert_eq!(posts(&mock), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn lost_create_that_never_landed_is_retried() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let (c, mock) = client(
            Mock::default()
                .then(HttpMethod::Post, INSTANCES, Err(TransportError::Timeout))
                .then(
                    HttpMethod::Post,
                    INSTANCES,
                    Ok(HttpResponse::new(200, PENDING_OP)),
                )
                .on(HttpMethod::Post, OP_WAIT, 200, DONE)
                .on(HttpMethod::Get, ONE, 404, "{}")
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", true, ""),
                ),
        );
        let inst = create(&c, &s, &rec, fast()).await.unwrap();
        assert!(matches!(inst.state(), VmState::Running { .. }));
        assert_eq!(posts(&mock), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn lost_create_finding_a_foreign_vm_is_an_error() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let (c, mock) = client(
            Mock::default()
                .then(HttpMethod::Post, INSTANCES, Err(TransportError::Timeout))
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", false, ""),
                ),
        );
        assert!(matches!(
            create(&c, &s, &rec, fast()).await,
            Err(GcpError::NotOwned { .. })
        ));
        assert_eq!(posts(&mock), 1);
    }

    #[tokio::test]
    async fn conflict_on_our_label_is_adopted_and_on_a_foreign_one_is_an_error() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let conflict = r#"{"error":{"code":409,"message":"already exists"}}"#;
        let (c, _) = client(
            Mock::default()
                .on(HttpMethod::Post, INSTANCES, 409, conflict)
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", true, ""),
                ),
        );
        assert!(create(&c, &s, &rec, fast()).await.is_ok());

        let (c, mock) = client(
            Mock::default()
                .on(HttpMethod::Post, INSTANCES, 409, conflict)
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", false, ""),
                ),
        );
        let e = create(&c, &s, &rec, fast()).await.unwrap_err();
        assert!(matches!(e, GcpError::Conflict { .. }), "{e:?}");
        assert!(
            mock.requests()
                .iter()
                .all(|r| r.method != HttpMethod::Delete)
        );
    }

    #[tokio::test]
    async fn stockout_in_the_operation_is_its_own_error() {
        for code in [
            "ZONE_RESOURCE_POOL_EXHAUSTED",
            "ZONE_RESOURCE_POOL_EXHAUSTED_WITH_DETAILS",
        ] {
            let s = the_spec();
            let rec = prepare(&s).unwrap();
            let op = format!(
                r#"{{"status":"DONE","error":{{"errors":[{{"code":"{code}","message":"no T4 left"}}]}}}}"#
            );
            let (c, _) = client(
                Mock::default()
                    .on(HttpMethod::Post, INSTANCES, 200, PENDING_OP)
                    .on(HttpMethod::Post, OP_WAIT, 200, &op),
            );
            let e = create(&c, &s, &rec, fast()).await.unwrap_err();
            assert_eq!(
                e,
                GcpError::ZoneExhausted {
                    zone: "us-central1-a".into(),
                    message: "no T4 left".into()
                }
            );
            assert!(e.is_stockout() && !e.is_retryable());
        }
    }

    #[tokio::test]
    async fn stockout_as_an_api_error_is_mapped_too() {
        let s = the_spec();
        let rec = prepare(&s).unwrap();
        let body = r#"{"error":{"code":503,"status":"UNAVAILABLE","message":"The zone 'projects/p/zones/z' does not have enough resources available to fulfill the request."}}"#;
        let (c, mock) = client(Mock::default().on(HttpMethod::Post, INSTANCES, 503, body));
        assert!(
            create(&c, &s, &rec, fast())
                .await
                .unwrap_err()
                .is_stockout()
        );
        assert_eq!(posts(&mock), 1);
    }

    #[test]
    fn preemption_is_read_from_the_message_or_the_stop_timestamp() {
        let parse = |j: &str| -> Instance { serde_json::from_str(j).unwrap() };
        let by_message =
            parse(r#"{"status":"TERMINATED","statusMessage":"Instance was preempted."}"#);
        assert_eq!(by_message.state(), VmState::Terminated { preempted: true });
        let by_stamp = parse(
            r#"{"status":"TERMINATED","lastStopTimestamp":"2026-10-22T10:00:00.000-07:00",
                "scheduling":{"provisioningModel":"SPOT"}}"#,
        );
        assert_eq!(by_stamp.state(), VmState::Terminated { preempted: true });
        let standard = parse(
            r#"{"status":"TERMINATED","lastStopTimestamp":"2026-10-22T10:00:00.000-07:00",
                "scheduling":{"provisioningModel":"STANDARD"}}"#,
        );
        assert_eq!(standard.state(), VmState::Terminated { preempted: false });
        for (s, want) in [
            ("PROVISIONING", VmState::Provisioning),
            ("STAGING", VmState::Staging),
            ("STOPPING", VmState::Stopping),
            ("SUSPENDED", VmState::Suspended),
            ("REPAIRING", VmState::Other("REPAIRING".into())),
        ] {
            assert_eq!(parse(&format!(r#"{{"status":"{s}"}}"#)).state(), want);
        }
    }

    #[tokio::test]
    async fn status_reads_running_with_ip_and_gone_on_404() {
        let rec = prepare(&the_spec()).unwrap();
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", true, ""),
        ));
        assert_eq!(
            status(&c, &rec).await.unwrap(),
            VmState::Running {
                external_ip: Some("203.0.113.7".into())
            }
        );
        let (c, _) = client(Mock::default().on(HttpMethod::Get, ONE, 404, "{}"));
        assert_eq!(status(&c, &rec).await.unwrap(), VmState::Gone);
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", false, ""),
        ));
        assert!(matches!(
            status(&c, &rec).await,
            Err(GcpError::NotOwned { .. })
        ));
    }

    #[tokio::test]
    async fn delete_refuses_an_unowned_vm_and_sends_no_delete() {
        let rec = prepare(&the_spec()).unwrap();
        let (c, mock) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            &instance_json("RUNNING", false, ""),
        ));
        assert!(matches!(
            delete(&c, &rec, fast()).await,
            Err(GcpError::NotOwned { .. })
        ));
        assert!(mock.requests().iter().all(|r| r.method == HttpMethod::Get));
        // A VM with no labels at all is just as foreign.
        let (c, _) = client(Mock::default().on(
            HttpMethod::Get,
            ONE,
            200,
            r#"{"name":"x","status":"RUNNING"}"#,
        ));
        assert!(matches!(
            delete(&c, &rec, fast()).await,
            Err(GcpError::NotOwned { .. })
        ));
        assert_eq!(OWNER_LABEL, "amicu-owner");
    }

    #[tokio::test(start_paused = true)]
    async fn delete_waits_for_the_operation_and_confirms_with_a_404() {
        let rec = prepare(&the_spec()).unwrap();
        let (c, mock) = client(
            Mock::default()
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("RUNNING", true, ""),
                )
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("STOPPING", true, ""),
                )
                .on(HttpMethod::Get, ONE, 404, "{}")
                .on(HttpMethod::Delete, ONE, 200, PENDING_OP)
                .on(HttpMethod::Post, OP_WAIT, 200, DONE),
        );
        assert_eq!(
            delete(&c, &rec, fast()).await.unwrap(),
            DeleteOutcome::Deleted
        );
        let methods: Vec<_> = mock.requests().iter().map(|r| r.method).collect();
        assert_eq!(methods[0], HttpMethod::Get);
        assert_eq!(methods[1], HttpMethod::Delete);
        assert_eq!(*methods.last().unwrap(), HttpMethod::Get);
    }

    #[tokio::test(start_paused = true)]
    async fn delete_that_never_disappears_times_out_and_absent_is_confirmed() {
        let rec = prepare(&the_spec()).unwrap();
        let (c, _) = client(
            Mock::default()
                .on(
                    HttpMethod::Get,
                    ONE,
                    200,
                    &instance_json("STOPPING", true, ""),
                )
                .on(HttpMethod::Delete, ONE, 200, PENDING_OP)
                .on(HttpMethod::Post, OP_WAIT, 200, DONE),
        );
        assert!(matches!(
            delete(&c, &rec, fast()).await,
            Err(GcpError::OperationTimeout { .. })
        ));
        let (c, mock) = client(Mock::default().on(HttpMethod::Get, ONE, 404, "{}"));
        assert_eq!(
            delete(&c, &rec, fast()).await.unwrap(),
            DeleteOutcome::AlreadyGone
        );
        assert_eq!(mock.requests().len(), 1);
    }
}
