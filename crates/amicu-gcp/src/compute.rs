//! Compute Engine VMs for a vLLM deployment: the `instances.insert` body
//! ([`spec`]), the boot-time startup script ([`startup`]), the VM lifecycle
//! ([`vm`]), the serving-port firewall rule ([`firewall`]) and the readiness
//! probe ([`health`]).
//!
//! Coexistence: the project may also be managed by other tools, so amicu only
//! changes or deletes resources that carry its owner marker (a label on the VM,
//! a description marker on the firewall rule, since firewall rules have no
//! labels), a `409` on a name it does not own is an error, and it never adopts
//! such a resource. Machine type, accelerator and zone arrive as validated
//! inputs: whether they exist together is the catalog's check, not this module's.
//!
//! Nothing here calls Google unless the caller's [`crate::GcpClient`] does.

pub mod firewall;
pub mod health;
pub mod spec;
pub mod startup;
pub mod vm;

pub use firewall::{FirewallSpec, create_firewall, delete_firewall, firewall_name};
pub use health::{HealthTiming, wait_ready};
pub use spec::{
    A4X_UNSUPPORTED, ARM64_IMAGE_FAMILY, Accelerator, DEFAULT_IMAGE_FAMILY, Provisioning, VmSpec,
    default_image_family, is_a4x_machine_type, is_arm_machine_type,
};
pub use startup::{SecretDelivery, StartupConfig};
pub use vm::{
    DeleteOutcome, Instance, VmRecord, VmState, create, delete, get, instance_name, prepare, status,
};
