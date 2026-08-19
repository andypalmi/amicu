//! The visible, inspectable launch artifact produced at planning time.

use amicu_core::ByteSize;
use amicu_core::id::DeploymentId;
use amicu_core::resource::Runtime;
use amicu_core::secret::SecretRef;
use serde::{Deserialize, Serialize};

use crate::capabilities::RuntimeKind;

/// The rendered launch instruction: a command line (llama.cpp) or a manifest
/// (vLLM), kept as inspectable text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "form", rename_all = "snake_case")]
pub enum RenderedLaunch {
    /// A command to exec directly: the structured argument vector (`argv[0]` is
    /// the program) plus a shell-quoted `text` for display only.
    ///
    /// Supervisors spawn from `argv` — never by shell-splitting `text` — so a
    /// path with spaces or metacharacters can neither mis-split nor inject.
    Command { argv: Vec<String>, text: String },
    /// A declarative manifest to submit to a scheduler.
    Manifest { text: String },
}

impl RenderedLaunch {
    /// The rendered text, for inspection before apply.
    #[must_use]
    pub fn as_text(&self) -> &str {
        match self {
            RenderedLaunch::Command { text, .. } | RenderedLaunch::Manifest { text } => text,
        }
    }

    /// The structured argument vector, when this is a [`RenderedLaunch::Command`].
    #[must_use]
    pub fn argv(&self) -> Option<&[String]> {
        match self {
            RenderedLaunch::Command { argv, .. } => Some(argv),
            RenderedLaunch::Manifest { .. } => None,
        }
    }
}

/// A planning-time estimate of the resources a launch will consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceEstimate {
    /// Estimated peak device memory (weights plus KV cache).
    pub estimated_memory: ByteSize,
    /// Number of accelerators the launch will occupy.
    pub gpu_count: u32,
}

/// The visible artifact of `plan_launch`: a rendered command/manifest, the
/// material launch settings, the secrets it references (by name only), and a
/// resource estimate.
///
/// A `LaunchPlan` is plan-safe: it embeds no secret values. Secrets appear
/// only as [`SecretRef`] metadata and are resolved to values at apply time via
/// a [`SecretResolver`](amicu_core::secret::SecretResolver).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LaunchPlan {
    /// The deployment this plan launches.
    pub deployment: DeploymentId,
    /// The runtime family that will serve it.
    pub runtime: RuntimeKind,
    /// The rendered, inspectable launch instruction.
    pub rendered: RenderedLaunch,
    /// The material launch settings carried through from the deployment.
    pub settings: Runtime,
    /// Secrets the launch will consume, referenced by name only.
    pub secret_refs: Vec<SecretRef>,
    /// The estimated resource footprint.
    pub estimate: ResourceEstimate,
}
