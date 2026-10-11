//! Canonical embedded agent-skill source.
//!
//! The markdown under `skills/` is the single source of truth; each file is
//! `include_str!`ed into this static manifest so `skill export` renders the
//! same content to every target with no drift and no extra dependency.

/// A reference file within a skill's `references/` directory.
pub(crate) struct SkillFile {
    pub name: &'static str,
    pub contents: &'static str,
}

/// One narrow skill: its `SKILL.md` plus concise reference files.
pub(crate) struct Skill {
    pub name: &'static str,
    pub md: &'static str,
    pub references: &'static [SkillFile],
}

macro_rules! reference {
    ($skill:literal, $file:literal) => {
        SkillFile {
            name: $file,
            contents: include_str!(concat!("../skills/", $skill, "/references/", $file)),
        }
    };
}

macro_rules! skill {
    ($name:literal, [$($file:literal),* $(,)?]) => {
        Skill {
            name: $name,
            md: include_str!(concat!("../skills/", $name, "/SKILL.md")),
            references: &[$(reference!($name, $file)),*],
        }
    };
}

/// The eight narrow skills, in a stable order.
pub(crate) const SKILLS: &[Skill] = &[
    skill!("amicu-core", ["resource-types.md", "cli-reference.md"]),
    skill!(
        "amicu-dsl",
        [
            "model-examples.md",
            "hardware-examples.md",
            "deployment-examples.md",
            "modal-deployment.md"
        ]
    ),
    skill!(
        "amicu-local",
        ["llama-server-args.md", "gguf-quantization-guide.md"]
    ),
    skill!("amicu-production", ["vllm-args.md", "helm-chart-config.md"]),
    skill!("amicu-model-selection", ["quality-cost-guide.md"]),
    skill!("amicu-routing", ["routing-examples.md"]),
    skill!("amicu-troubleshooting", ["error-codes.md"]),
    skill!("amicu-presets", ["preset-catalog.md"]),
];
