//! Compute Engine VMs for a vLLM deployment: the `instances.insert` body
//! ([`spec`]), the boot-time startup script ([`startup`]), the VM lifecycle
//! ([`vm`]), the serving-port firewall rule ([`firewall`]) and the readiness
//! probe ([`health`]), TLS with a pinned certificate ([`tls`]) and the VM's
//! Secret Manager secrets ([`secure`]).
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
pub mod secure;
pub mod spec;
pub mod startup;
pub mod tls;
pub mod vm;

pub use firewall::{FirewallSpec, create_firewall, delete_firewall, firewall_name};
pub use health::{HealthTiming, StartupReport, probe_transport, startup_report, wait_ready};
pub use secure::{delete_secrets, provision_secrets};
pub use spec::{
    A4X_UNSUPPORTED, ARM64_IMAGE_FAMILY, Accelerator, DEFAULT_IMAGE_FAMILY, Provisioning, VmSpec,
    default_image_family, is_a4x_machine_type, is_arm_machine_type,
};
pub use startup::{SecretDelivery, StartupConfig};
pub use tls::{PinnedTransport, TlsIdentity, cert_path, fingerprint_of_pem, write_cert_pem};
pub use vm::{
    DeleteOutcome, Instance, VmRecord, VmState, create, delete, get, guest_attributes,
    instance_name, prepare, status,
};
