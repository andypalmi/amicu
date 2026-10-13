//! `llama-server` binary detection and version checking.
//!
//! Command execution is abstracted behind [`CommandRunner`] so detection logic
//! stays pure and is unit-tested against canned `--version` output; [`HostRunner`]
//! runs the real binary, tests inject a fake.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::LlamaCppError;

/// A parsed `llama-server` version, keyed on its monotonic build number
/// (`version: <build> (<sha>)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct LlamaVersion {
    /// The build number llama.cpp stamps into each release.
    pub build: u32,
}

impl LlamaVersion {
    /// A version from its build number.
    #[must_use]
    pub const fn new(build: u32) -> Self {
        Self { build }
    }

    /// Parse the build number from `llama-server --version` output.
    ///
    /// Accepts the `version: 3901 (a1b2c3d)` line llama.cpp emits, tolerating a
    /// leading `b` (e.g. `b3901`) and extra surrounding text.
    ///
    /// # Errors
    /// [`LlamaCppError::UnparseableVersion`] when no build number is present.
    pub fn parse(output: &str) -> Result<Self, LlamaCppError> {
        output
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                let rest = lower.split("version").nth(1)?;
                let digits: String = rest
                    .chars()
                    .skip_while(|c| !c.is_ascii_digit())
                    .take_while(char::is_ascii_digit)
                    .collect();
                digits.parse().ok().map(LlamaVersion::new)
            })
            .ok_or_else(|| LlamaCppError::UnparseableVersion(output.to_string()))
    }
}

impl std::fmt::Display for LlamaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", self.build)
    }
}

/// Abstracts locating and probing the binary so detection stays testable.
pub trait CommandRunner: Send + Sync {
    /// Resolve `name` on `PATH`, returning its full path when found.
    fn which(&self, name: &str) -> Option<PathBuf>;
    /// Run `path --version`, returning its combined stdout/stderr.
    fn version_output(&self, path: &Path) -> Result<String, LlamaCppError>;
}

/// The real runner: consults `PATH` and executes the binary.
#[derive(Debug, Default, Clone, Copy)]
pub struct HostRunner;

impl CommandRunner for HostRunner {
    fn which(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path).find_map(|dir| {
            let candidate = dir.join(name);
            is_executable(&candidate).then_some(candidate)
        })
    }

    fn version_output(&self, path: &Path) -> Result<String, LlamaCppError> {
        let output = Command::new(path)
            .arg("--version")
            .output()
            .map_err(|e| LlamaCppError::Probe(e.to_string()))?;
        // llama.cpp prints its version banner to stderr; merge both streams.
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&output.stderr));
        Ok(text)
    }
}

/// A regular file with an execute bit (unix); any regular file elsewhere.
fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

/// How to locate and version-gate the binary.
#[derive(Debug, Clone)]
pub struct BinaryConfig {
    /// An explicit binary path; falls back to a `PATH` search when `None`.
    pub path: Option<PathBuf>,
    /// The binary name searched on `PATH`.
    pub name: String,
    /// The lowest acceptable version.
    pub minimum: LlamaVersion,
}

impl Default for BinaryConfig {
    fn default() -> Self {
        Self {
            path: None,
            name: "llama-server".to_string(),
            // A recent-enough build carrying the server flags this adapter renders.
            minimum: LlamaVersion::new(3000),
        }
    }
}

/// A located, version-checked binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryInfo {
    /// The resolved binary path.
    pub path: PathBuf,
    /// The parsed version.
    pub version: LlamaVersion,
}

/// Locate `llama-server`, probe its version, and gate it on `cfg.minimum`.
///
/// # Errors
/// - [`LlamaCppError::BinaryNotFound`] when neither the configured path nor
///   `PATH` yields the binary.
/// - [`LlamaCppError::VersionTooOld`] when the binary is older than the minimum.
/// - [`LlamaCppError::Probe`]/[`LlamaCppError::UnparseableVersion`] on probe
///   or parse failure.
pub fn detect_binary(
    runner: &dyn CommandRunner,
    cfg: &BinaryConfig,
) -> Result<BinaryInfo, LlamaCppError> {
    let path = cfg
        .path
        .clone()
        .or_else(|| runner.which(&cfg.name))
        .ok_or(LlamaCppError::BinaryNotFound)?;
    let version = LlamaVersion::parse(&runner.version_output(&path)?)?;
    if version < cfg.minimum {
        return Err(LlamaCppError::VersionTooOld {
            found: version,
            minimum: cfg.minimum,
        });
    }
    Ok(BinaryInfo { path, version })
}
