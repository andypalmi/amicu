//! The `instances.insert` request body for a vLLM VM.
//!
//! Field names are checked against Compute Engine's v1 discovery document
//! (`scheduling.provisioningModel` is `SPOT` / `FLEX_START` / `STANDARD`,
//! `scheduling.maxRunDuration` is a `Duration` with a `seconds` string, flex-start
//! waits for capacity for `params.requestValidForDuration`).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::startup::{SecretDelivery, StartupConfig, startup_metadata};
use crate::error::GcpError;
use crate::labels::OWNER_LABEL;
use crate::validate::{validate_project_id, validate_resource_name, validate_zone};

/// The Deep Learning VM image family used for the boot disk: Ubuntu 22.04 with
/// CUDA 12.9 and the NVIDIA 580 driver preinstalled, in project
/// `deeplearning-platform-release`.
pub const DEFAULT_IMAGE_FAMILY: &str = "common-cu129-ubuntu-2204-nvidia-580";
/// The project that publishes the Deep Learning VM images.
pub const IMAGE_PROJECT: &str = "deeplearning-platform-release";

const SCOPE_CLOUD_PLATFORM: &str = "https://www.googleapis.com/auth/cloud-platform";
const MIN_RUN: Duration = Duration::from_mins(10);
const MAX_FLEX_RUN: Duration = Duration::from_hours(7 * 24);
const MAX_RUN: Duration = Duration::from_hours(120 * 24);
const MAX_FLEX_WAIT: Duration = Duration::from_hours(2);

/// How the VM is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provisioning {
    /// Spot VM: cheap, can be preempted (then deleted).
    Spot,
    /// Flex-start VM: queued for capacity for up to `wait`, then runs for at
    /// most the spec's `max_run`, which must be 7 days or less.
    FlexStart { wait: Duration },
    /// On-demand VM.
    Standard,
}

/// A GPU attached to an N1 VM. A2, A3 and G2 machine types have their GPUs
/// built in and take none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Accelerator {
    /// Accelerator type name, for example `nvidia-tesla-t4`.
    pub type_name: String,
    pub count: u32,
}

/// Everything the VM needs. All identifiers are validated by [`VmSpec::validate`].
#[derive(Debug, Clone)]
pub struct VmSpec {
    pub project: String,
    pub zone: String,
    /// Instance name (see [`super::vm::instance_name`]).
    pub name: String,
    pub machine_type: String,
    /// Only for machine types without built-in GPUs (N1).
    pub accelerator: Option<Accelerator>,
    pub provisioning: Provisioning,
    /// The backstop: Compute Engine deletes the VM after this long whatever
    /// else happens.
    pub max_run: Duration,
    pub boot_disk_gb: u32,
    /// Deep Learning VM image family; see [`DEFAULT_IMAGE_FAMILY`].
    pub image_family: String,
    /// Also set the `install-nvidia-driver` metadata key. The default image
    /// families ship the driver, so this is only for older families.
    pub install_nvidia_driver: bool,
    /// Runtime service account email; `None` attaches no service account.
    /// Required with [`SecretDelivery::SecretManager`]: it is the identity that
    /// reads the secrets (amicu grants it access on each secret, nothing more,
    /// and never creates it or changes project IAM).
    pub service_account: Option<String>,
    /// The network tag the firewall rule targets.
    pub network_tag: String,
    /// Labels from [`crate::owner_labels`]; must carry [`OWNER_LABEL`].
    pub labels: BTreeMap<String, String>,
    pub startup: StartupConfig,
}

impl VmSpec {
    /// The owner marker from the labels.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] when the labels have none.
    pub fn owner(&self) -> Result<&str, GcpError> {
        self.labels
            .get(OWNER_LABEL)
            .map(String::as_str)
            .ok_or_else(|| invalid("labels", OWNER_LABEL, "the owner label is required"))
    }

    /// Checks identifiers, durations and the startup config.
    ///
    /// # Errors
    /// [`GcpError::Invalid`] naming the first bad field.
    pub fn validate(&self) -> Result<(), GcpError> {
        validate_project_id(&self.project)?;
        validate_zone(&self.zone)?;
        validate_resource_name(&self.name)?;
        validate_resource_name(&self.network_tag)?;
        self.owner()?;
        token("machine type", &self.machine_type)?;
        token("image family", &self.image_family)?;
        if let Some(a) = &self.accelerator {
            token("accelerator type", &a.type_name)?;
            if a.count == 0 {
                return Err(invalid("accelerator count", "0", "must be at least 1"));
            }
        }
        let email_like = |sa: &str| sa.contains('@') && !sa.contains(char::is_whitespace);
        if let Some(sa) = &self.service_account
            && !email_like(sa)
        {
            return Err(invalid("service account", sa, "expected an email address"));
        }
        if self.boot_disk_gb < 50 {
            return Err(invalid(
                "boot disk size",
                &self.boot_disk_gb.to_string(),
                "the GPU images need at least 50 GB",
            ));
        }
        let flex = matches!(self.provisioning, Provisioning::FlexStart { .. });
        let max = if flex { MAX_FLEX_RUN } else { MAX_RUN };
        if self.max_run < MIN_RUN || self.max_run > max {
            return Err(invalid(
                "max run duration",
                &format!("{}s", self.max_run.as_secs()),
                "must be between 10 minutes and 7 days (120 days unless flex-start)",
            ));
        }
        if let Provisioning::FlexStart { wait } = self.provisioning
            && (wait.is_zero() || wait > MAX_FLEX_WAIT)
        {
            return Err(invalid(
                "flex-start wait",
                &format!("{}s", wait.as_secs()),
                "must be between 1 second and 2 hours",
            ));
        }
        if self.startup.delivery == SecretDelivery::SecretManager && self.service_account.is_none()
        {
            return Err(invalid(
                "service account",
                "",
                "required to read secrets from Secret Manager",
            ));
        }
        self.startup.validate()
    }

    /// The `instances.insert` request body.
    ///
    /// # Errors
    /// As [`validate`](Self::validate).
    pub fn insert_body(&self) -> Result<Value, GcpError> {
        self.validate()?;
        let zone = &self.zone;
        let mut body = json!({
            "name": self.name,
            "machineType": format!("zones/{zone}/machineTypes/{}", self.machine_type),
            "labels": self.labels,
            "tags": { "items": [self.network_tag] },
            "disks": [{
                "boot": true,
                "autoDelete": true,
                "type": "PERSISTENT",
                "initializeParams": {
                    "sourceImage": format!(
                        "projects/{IMAGE_PROJECT}/global/images/family/{}",
                        self.image_family
                    ),
                    "diskSizeGb": self.boot_disk_gb.to_string(),
                    "diskType": format!("zones/{zone}/diskTypes/pd-balanced"),
                },
            }],
            "networkInterfaces": [{
                "network": "global/networks/default",
                "accessConfigs": [{
                    "name": "External NAT",
                    "type": "ONE_TO_ONE_NAT",
                    "networkTier": "PREMIUM",
                }],
            }],
            "scheduling": self.scheduling(),
            "serviceAccounts": self.service_accounts(),
            "metadata": { "items": startup_metadata(
                &self.startup,
                &self.project,
                &self.name,
                self.install_nvidia_driver
            ) },
        });
        if let Some(a) = &self.accelerator {
            body["guestAccelerators"] = json!([{
                "acceleratorType": format!("zones/{zone}/acceleratorTypes/{}", a.type_name),
                "acceleratorCount": a.count,
            }]);
        }
        if let Provisioning::FlexStart { wait } = self.provisioning {
            body["params"] = json!({ "requestValidForDuration": duration(wait) });
            body["reservationAffinity"] = json!({ "consumeReservationType": "NO_RESERVATION" });
        }
        Ok(body)
    }

    /// GPU VMs cannot live-migrate, so `onHostMaintenance` is always
    /// `TERMINATE`, and `maxRunDuration` with `DELETE` is always set.
    fn scheduling(&self) -> Value {
        let model = match self.provisioning {
            Provisioning::Spot => "SPOT",
            Provisioning::FlexStart { .. } => "FLEX_START",
            Provisioning::Standard => "STANDARD",
        };
        json!({
            "provisioningModel": model,
            "onHostMaintenance": "TERMINATE",
            "automaticRestart": false,
            "instanceTerminationAction": "DELETE",
            "maxRunDuration": duration(self.max_run),
        })
    }

    /// No service account unless one is named; with one, the broad scope, so
    /// that IAM on the account decides what the VM can do.
    fn service_accounts(&self) -> Value {
        match &self.service_account {
            Some(email) => json!([{ "email": email, "scopes": [SCOPE_CLOUD_PLATFORM] }]),
            None => json!([]),
        }
    }
}

fn duration(d: Duration) -> Value {
    json!({ "seconds": d.as_secs().to_string() })
}

fn invalid(kind: &'static str, value: &str, reason: &'static str) -> GcpError {
    GcpError::Invalid {
        kind,
        value: value.to_owned(),
        reason,
    }
}

/// A machine type, accelerator type or image family: lowercase letters, digits
/// and hyphens, safe in a URL path.
fn token(kind: &'static str, v: &str) -> Result<(), GcpError> {
    if !v.is_empty()
        && v.len() <= 63
        && v.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        Ok(())
    } else {
        Err(invalid(
            kind,
            v,
            "only lowercase letters, digits and hyphens",
        ))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use amicu_core::secret::SecretValue;

    use super::*;
    use crate::labels::owner_labels;

    pub(crate) fn spec(machine: &str, acc: Option<(&str, u32)>, p: Provisioning) -> VmSpec {
        VmSpec {
            project: "my-proj".into(),
            zone: "us-central1-a".into(),
            name: "amicu-demo-chat-0123456789ab-n1".into(),
            machine_type: machine.into(),
            accelerator: acc.map(|(t, c)| Accelerator {
                type_name: t.into(),
                count: c,
            }),
            provisioning: p,
            max_run: Duration::from_hours(6),
            boot_disk_gb: 100,
            image_family: DEFAULT_IMAGE_FAMILY.into(),
            install_nvidia_driver: false,
            service_account: None,
            network_tag: "amicu-vllm".into(),
            labels: owner_labels(None, "demo", "chat"),
            startup: StartupConfig {
                container_image: "vllm/vllm-openai:v0.11.0".into(),
                vllm_args: vec!["--model".into(), "Qwen/Qwen3-0.6B".into()],
                serving_port: 8000,
                api_key: SecretValue::new("sk-test-key"),
                hf_token: Some(SecretValue::new("hf_test")),
                delivery: SecretDelivery::InstanceMetadata,
                tls: None,
            },
        }
    }

    fn sched(spec: &VmSpec) -> Value {
        spec.insert_body().unwrap()["scheduling"].clone()
    }

    #[test]
    fn spot_t4_on_n1_attaches_the_accelerator() {
        let s = spec(
            "n1-standard-4",
            Some(("nvidia-tesla-t4", 1)),
            Provisioning::Spot,
        );
        let b = s.insert_body().unwrap();
        assert_eq!(
            b["machineType"],
            "zones/us-central1-a/machineTypes/n1-standard-4"
        );
        assert_eq!(
            b["guestAccelerators"],
            json!([{
                "acceleratorType": "zones/us-central1-a/acceleratorTypes/nvidia-tesla-t4",
                "acceleratorCount": 1
            }])
        );
        assert_eq!(
            b["scheduling"],
            json!({
                "provisioningModel": "SPOT",
                "onHostMaintenance": "TERMINATE",
                "automaticRestart": false,
                "instanceTerminationAction": "DELETE",
                "maxRunDuration": {"seconds": "21600"},
            })
        );
        assert_eq!(b["tags"], json!({"items": ["amicu-vllm"]}));
        assert_eq!(b["labels"][OWNER_LABEL], s.owner().unwrap());
        assert_eq!(b["serviceAccounts"], json!([]));
        assert_eq!(
            b["networkInterfaces"][0]["accessConfigs"][0]["type"],
            "ONE_TO_ONE_NAT"
        );
        assert_eq!(
            b["networkInterfaces"][0]["network"],
            "global/networks/default"
        );
        let disk = &b["disks"][0];
        assert_eq!(disk["boot"], true);
        assert_eq!(disk["autoDelete"], true);
        assert_eq!(disk["initializeParams"]["diskSizeGb"], "100");
        assert_eq!(
            disk["initializeParams"]["sourceImage"],
            "projects/deeplearning-platform-release/global/images/family/common-cu129-ubuntu-2204-nvidia-580"
        );
        assert!(b.get("params").is_none());
    }

    #[test]
    fn built_in_gpu_machines_carry_no_guest_accelerators() {
        for machine in ["g2-standard-4", "a3-highgpu-1g"] {
            let b = spec(machine, None, Provisioning::Spot)
                .insert_body()
                .unwrap();
            assert!(b.get("guestAccelerators").is_none(), "{machine}");
            assert_eq!(b["scheduling"]["provisioningModel"], "SPOT");
            assert_eq!(b["scheduling"]["onHostMaintenance"], "TERMINATE");
        }
        let b = spec("a3-highgpu-1g", None, Provisioning::Spot)
            .insert_body()
            .unwrap();
        assert_eq!(
            b["machineType"],
            "zones/us-central1-a/machineTypes/a3-highgpu-1g"
        );
    }

    #[test]
    fn flex_start_waits_for_capacity_and_has_no_reservation() {
        let s = spec(
            "g2-standard-4",
            None,
            Provisioning::FlexStart {
                wait: Duration::from_mins(30),
            },
        );
        let b = s.insert_body().unwrap();
        assert_eq!(b["scheduling"]["provisioningModel"], "FLEX_START");
        assert_eq!(b["scheduling"]["instanceTerminationAction"], "DELETE");
        assert_eq!(
            b["scheduling"]["maxRunDuration"],
            json!({"seconds": "21600"})
        );
        assert_eq!(
            b["params"]["requestValidForDuration"],
            json!({"seconds": "1800"})
        );
        assert_eq!(
            b["reservationAffinity"],
            json!({"consumeReservationType": "NO_RESERVATION"})
        );
    }

    #[test]
    fn standard_keeps_the_backstop_and_terminate_policy() {
        let s = sched(&spec("g2-standard-4", None, Provisioning::Standard));
        assert_eq!(s["provisioningModel"], "STANDARD");
        assert_eq!(s["onHostMaintenance"], "TERMINATE");
        assert_eq!(s["instanceTerminationAction"], "DELETE");
        assert!(s["maxRunDuration"]["seconds"].is_string());
    }

    #[test]
    fn service_account_is_opt_in() {
        let mut s = spec("g2-standard-4", None, Provisioning::Spot);
        s.service_account = Some("vm@my-proj.iam.gserviceaccount.com".into());
        assert_eq!(
            s.insert_body().unwrap()["serviceAccounts"][0]["email"],
            "vm@my-proj.iam.gserviceaccount.com"
        );
    }

    #[test]
    fn driver_metadata_key_is_opt_in() {
        let keys = |s: &VmSpec| -> Vec<String> {
            s.insert_body().unwrap()["metadata"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i["key"].as_str().unwrap().to_owned())
                .collect()
        };
        let mut s = spec("g2-standard-4", None, Provisioning::Spot);
        assert!(!keys(&s).contains(&"install-nvidia-driver".to_owned()));
        s.install_nvidia_driver = true;
        assert!(keys(&s).contains(&"install-nvidia-driver".to_owned()));
        assert!(keys(&s).contains(&"startup-script".to_owned()));
    }

    #[test]
    fn rejects_bad_inputs() {
        let base = spec("g2-standard-4", None, Provisioning::Spot);
        let mut s = base.clone();
        s.labels.clear();
        assert!(s.insert_body().is_err());
        let mut s = base.clone();
        s.max_run = Duration::from_secs(5);
        assert!(s.insert_body().is_err());
        let mut s = base.clone();
        s.machine_type = "g2/../x".into();
        assert!(s.insert_body().is_err());
        let mut s = base.clone();
        s.provisioning = Provisioning::FlexStart {
            wait: Duration::from_mins(5),
        };
        s.max_run = Duration::from_hours(8 * 24);
        assert!(s.insert_body().is_err());
        let mut s = base;
        s.service_account = Some("not-an-email".into());
        assert!(s.insert_body().is_err());
    }
}
