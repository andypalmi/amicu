//! The `instances.insert` request body for a vLLM VM.
//!
//! Field names are checked against Compute Engine's v1 discovery document
//! (`scheduling.provisioningModel` is `SPOT` / `FLEX_START` / `STANDARD`,
//! `scheduling.maxRunDuration` is a `Duration` with a `seconds` string, flex-start
//! waits for capacity for `params.requestValidForDuration`).

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};

use super::startup::{StartupConfig, startup_metadata};
use crate::error::GcpError;
use crate::labels::OWNER_LABEL;
use crate::validate::{validate_project_id, validate_resource_name, validate_zone};

/// The Deep Learning VM image family used for the boot disk: Ubuntu 22.04 with
/// CUDA 12.9 and the NVIDIA 580 driver preinstalled, in project
/// `deeplearning-platform-release`.
pub const DEFAULT_IMAGE_FAMILY: &str = "common-cu129-ubuntu-2204-nvidia-580";
/// The project that publishes the Deep Learning VM images.
pub const IMAGE_PROJECT: &str = "deeplearning-platform-release";
/// The boot image family for the Arm machine types (A4X and A4X Max, NVIDIA
/// Grace CPUs): Ubuntu 24.04 arm64 with the NVIDIA 580 driver, from Google's
/// accelerator OS images. The Deep Learning VM project publishes no arm64
/// family, so these machines cannot use [`DEFAULT_IMAGE_FAMILY`].
pub const ARM64_IMAGE_FAMILY: &str = "ubuntu-accelerator-2404-arm64-with-nvidia-580";
/// The project that publishes [`ARM64_IMAGE_FAMILY`].
const UBUNTU_ACCELERATOR_PROJECT: &str = "ubuntu-os-accelerator-images";
/// The project that publishes the Rocky Linux accelerator images.
const ROCKY_ACCELERATOR_PROJECT: &str = "rocky-linux-accelerator-cloud";

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
    /// Boot image family; see [`default_image_family`] for the right one for a
    /// machine type. Its CPU architecture must match the machine type.
    pub image_family: String,
    /// Also set the `install-nvidia-driver` metadata key. The default image
    /// families ship the driver, so this is only for older families.
    pub install_nvidia_driver: bool,
    /// Runtime service account email; `None` attaches no service account.
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

    /// The boot disk type for the machine series: `hyperdisk-balanced` for the
    /// series that cannot use Persistent Disk (G4, A4, A4X and A3 Ultra),
    /// `pd-balanced` for the rest. See [`boot_disk_type`].
    #[must_use]
    pub fn boot_disk_type(&self) -> &'static str {
        boot_disk_type(&self.machine_type)
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
        let arm_machine = is_arm_machine_type(&self.machine_type);
        if arm_machine != self.image_family.contains("arm64") {
            return Err(invalid(
                "image family",
                &self.image_family,
                if arm_machine {
                    "A4X machine types are Arm and need an arm64 image family"
                } else {
                    "an arm64 image family needs an Arm machine type (A4X)"
                },
            ));
        }
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
                "must be between 10 minutes and 120 days (7 days for flex-start)",
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
                        "projects/{}/global/images/family/{}",
                        image_project(&self.image_family),
                        self.image_family
                    ),
                    "diskSizeGb": self.boot_disk_gb.to_string(),
                    "diskType": format!("zones/{zone}/diskTypes/{}", self.boot_disk_type()),
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
            "metadata": { "items": startup_metadata(&self.startup, self.install_nvidia_driver) },
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

/// Whether a machine type runs on Arm CPUs. Of the GPU series, only A4X and
/// A4X Max (NVIDIA Grace) do; everything else is x86.
#[must_use]
pub fn is_arm_machine_type(machine_type: &str) -> bool {
    machine_type.starts_with("a4x-")
}

/// The boot image family for a machine type: [`ARM64_IMAGE_FAMILY`] for Arm
/// machines, else the x86 Deep Learning VM [`DEFAULT_IMAGE_FAMILY`].
#[must_use]
pub fn default_image_family(machine_type: &str) -> &'static str {
    if is_arm_machine_type(machine_type) {
        ARM64_IMAGE_FAMILY
    } else {
        DEFAULT_IMAGE_FAMILY
    }
}

/// The project publishing an image family: the accelerator OS image projects
/// for their families, else the Deep Learning VM project.
fn image_project(family: &str) -> &'static str {
    if family.starts_with("ubuntu-accelerator-") {
        UBUNTU_ACCELERATOR_PROJECT
    } else if family.starts_with("rocky-linux-") {
        ROCKY_ACCELERATOR_PROJECT
    } else {
        IMAGE_PROJECT
    }
}

/// The boot disk type a machine type can use. G4, A4, A4X (and A4X Max) and
/// A3 Ultra support Hyperdisk only; every other GPU series (N1, A2, A3 High,
/// Mega and Edge, G2) boots from `pd-balanced`.
#[must_use]
pub fn boot_disk_type(machine_type: &str) -> &'static str {
    const HYPERDISK_ONLY: [&str; 4] = ["g4-", "a4-", "a4x-", "a3-ultragpu-"];
    if HYPERDISK_ONLY.iter().any(|p| machine_type.starts_with(p)) {
        "hyperdisk-balanced"
    } else {
        "pd-balanced"
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
    fn a4x_boots_an_arm64_image_and_the_rest_x86() {
        assert!(is_arm_machine_type("a4x-highgpu-4g"));
        assert!(is_arm_machine_type("a4x-maxgpu-4g-metal"));
        for m in [
            "a4-highgpu-8g",
            "a3-ultragpu-8g",
            "g4-standard-48",
            "g2-standard-4",
        ] {
            assert!(!is_arm_machine_type(m), "{m}");
            assert_eq!(default_image_family(m), DEFAULT_IMAGE_FAMILY, "{m}");
        }
        for m in ["a4x-highgpu-4g", "a4x-maxgpu-4g-metal"] {
            assert_eq!(default_image_family(m), ARM64_IMAGE_FAMILY);
            let mut s = spec(m, None, Provisioning::Standard);
            s.image_family = default_image_family(m).into();
            assert_eq!(
                s.insert_body().unwrap()["disks"][0]["initializeParams"]["sourceImage"],
                "projects/ubuntu-os-accelerator-images/global/images/family/ubuntu-accelerator-2404-arm64-with-nvidia-580"
            );
        }
    }

    #[test]
    fn the_image_architecture_must_match_the_machine() {
        // x86 image on an Arm machine, and the reverse.
        let a4x = spec("a4x-highgpu-4g", None, Provisioning::Standard);
        assert!(a4x.validate().unwrap_err().to_string().contains("arm64"));
        let mut g2 = spec("g2-standard-4", None, Provisioning::Standard);
        g2.image_family = ARM64_IMAGE_FAMILY.into();
        assert!(g2.validate().is_err());
        // The Rocky arm64 family resolves to its own project.
        let mut s = spec("a4x-highgpu-4g", None, Provisioning::Standard);
        s.image_family = "rocky-linux-9-optimized-gcp-nvidia-580-arm64".into();
        assert!(
            s.insert_body().unwrap()["disks"][0]["initializeParams"]["sourceImage"]
                .as_str()
                .unwrap()
                .starts_with("projects/rocky-linux-accelerator-cloud/")
        );
    }

    #[test]
    fn boot_disk_type_follows_the_machine_series() {
        for m in [
            "g4-standard-48",
            "a4-highgpu-8g",
            "a4x-highgpu-4g",
            "a4x-maxgpu-4g-metal",
            "a3-ultragpu-8g",
        ] {
            assert_eq!(boot_disk_type(m), "hyperdisk-balanced", "{m}");
        }
        for m in [
            "n1-standard-4",
            "g2-standard-4",
            "a2-highgpu-1g",
            "a3-highgpu-8g",
            "a3-megagpu-8g",
            "a3-edgegpu-8g",
        ] {
            assert_eq!(boot_disk_type(m), "pd-balanced", "{m}");
        }
        let disk_type = |m: &str| {
            spec(m, None, Provisioning::Spot).insert_body().unwrap()["disks"][0]["initializeParams"]
                ["diskType"]
                .clone()
        };
        assert_eq!(
            disk_type("g4-standard-48"),
            "zones/us-central1-a/diskTypes/hyperdisk-balanced"
        );
        let n1 = spec(
            "n1-standard-4",
            Some(("nvidia-tesla-t4", 1)),
            Provisioning::Spot,
        );
        assert_eq!(
            n1.insert_body().unwrap()["disks"][0]["initializeParams"]["diskType"],
            "zones/us-central1-a/diskTypes/pd-balanced"
        );
    }

    #[test]
    fn max_run_limits_in_the_message_are_the_right_way_round() {
        let mut s = spec("g2-standard-4", None, Provisioning::Spot);
        s.max_run = Duration::from_hours(121 * 24);
        let e = s.validate().unwrap_err().to_string();
        assert!(e.contains("120 days (7 days for flex-start)"), "{e}");
        s.max_run = Duration::from_hours(100 * 24);
        assert!(s.validate().is_ok());
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
