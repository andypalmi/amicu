//! Change detection for inputs that are not part of the config graph.
//!
//! Two plans of the same graph can still differ in what actually runs: a
//! secret reference re-pinned to another version, the container image the
//! operator selected through the environment, or the binary a host
//! deployment resolves on `PATH`. A [`Fingerprint`] captures those inputs as
//! digests (never the raw values: an image reference can embed registry
//! credentials), is recorded in state when a deployment is confirmed started,
//! and is compared on the next plan.
//!
//! Unknown is never a change: a component that cannot be determined now, or
//! that was not recorded, is skipped, so detection can only add an Update for
//! a proven difference and never invents one (and never drops a record).

use amicu_core::resource::{Deployment, HardwareProfile};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// What an [`EnvProbe`] knows about one input of one deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fact {
    /// The input does not apply to this deployment (an image for a host
    /// deployment, a binary for a Modal one).
    NotApplicable,
    /// It applies but cannot be determined now (binary not on `PATH`).
    Unknown,
    /// Its current value.
    Known(String),
}

/// Reads the environment-dependent inputs of a deployment. The engine is
/// side-effect-free, so the caller supplies this (the CLI reads the process
/// environment and `PATH`; tests supply a fake).
pub trait EnvProbe {
    /// The container image a deployment will run in (Modal).
    fn image(&self, deployment: &Deployment, hardware: Option<&HardwareProfile>) -> Fact;
    /// The absolute path of the binary a deployment will run (host).
    fn binary(&self, deployment: &Deployment, hardware: Option<&HardwareProfile>) -> Fact;
}

/// A probe that knows nothing: image and binary are never compared.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoProbe;

impl EnvProbe for NoProbe {
    fn image(&self, _: &Deployment, _: Option<&HardwareProfile>) -> Fact {
        Fact::Unknown
    }
    fn binary(&self, _: &Deployment, _: Option<&HardwareProfile>) -> Fact {
        Fact::Unknown
    }
}

/// Digests of the out-of-graph inputs of one deployment. `None` means not
/// applicable, unknown, or not recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// Secret references (name, scope, pinned version).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<String>,
    /// The container image reference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The resolved binary path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    /// The rendered launch command (arguments), so a change to how amicu starts
    /// the runtime shows as an update even when the HCL is unchanged. Absent in
    /// state written before it was recorded: that is not a difference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
}

fn digest(label: &str, value: &str) -> String {
    let mut h = Sha256::new();
    h.update(label.as_bytes());
    h.update([0]);
    h.update(value.as_bytes());
    h.finalize().iter().fold(String::new(), |mut s, b| {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    })
}

fn fact_digest(label: &str, fact: Fact) -> Option<String> {
    match fact {
        Fact::Known(v) => Some(digest(label, &v)),
        Fact::NotApplicable | Fact::Unknown => None,
    }
}

impl Fingerprint {
    /// The fingerprint of `d` as the environment is now.
    #[must_use]
    pub fn compute(
        d: &Deployment,
        hardware: Option<&HardwareProfile>,
        probe: &dyn EnvProbe,
    ) -> Self {
        // `None` and "current version" differ from an explicit pin on purpose.
        let secrets = d.api_key.as_ref().map_or_else(String::new, |r| {
            format!(
                "api_key={}|{}|{}",
                r.name,
                r.scope.as_deref().unwrap_or("-"),
                r.version.as_deref().unwrap_or("-")
            )
        });
        Self {
            secrets: Some(digest("secrets", &secrets)),
            image: fact_digest("image", probe.image(d, hardware)),
            binary: fact_digest("binary", probe.binary(d, hardware)),
            launch: None,
        }
    }

    /// This fingerprint with the digest of the rendered launch command `text`.
    #[must_use]
    pub fn with_launch(mut self, text: &str) -> Self {
        self.launch = Some(digest("launch", text));
        self
    }

    /// The inputs that provably differ from what was `recorded` when the
    /// deployment was last confirmed started. A component missing on either
    /// side is not a difference.
    #[must_use]
    pub fn changes_from(&self, recorded: &Self) -> Vec<&'static str> {
        let differs = |now: &Option<String>, then: &Option<String>| matches!((now, then), (Some(a), Some(b)) if a != b);
        let mut out = Vec::new();
        if differs(&self.secrets, &recorded.secrets) {
            out.push("secret references");
        }
        if differs(&self.image, &recorded.image) {
            out.push("container image");
        }
        if differs(&self.binary, &recorded.binary) {
            out.push("binary path");
        }
        if differs(&self.launch, &recorded.launch) {
            out.push("launch arguments");
        }
        out
    }

    /// Fill components not yet recorded from `current`, keeping every recorded
    /// one. Used to adopt a baseline for a deployment that is unchanged.
    pub fn fill_missing(&mut self, current: &Self) {
        self.secrets = self.secrets.take().or_else(|| current.secrets.clone());
        self.image = self.image.take().or_else(|| current.image.clone());
        self.binary = self.binary.take().or_else(|| current.binary.clone());
        self.launch = self.launch.take().or_else(|| current.launch.clone());
    }
}
