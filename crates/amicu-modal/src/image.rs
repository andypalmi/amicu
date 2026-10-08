//! Container-image definitions for Modal's `ImageGetOrCreate` builder.
//!
//! Modal's Python client builds an image as a *chain* of one-command layers:
//! `Image.from_registry(tag)` emits `["FROM <tag>", …]`
//! (`modal/image.py:1887-1892` via `_registry_setup_commands` at `:1846-1892`),
//! and every `.entrypoint()` / `.pip_install()` / `.env()` call creates a new
//! `_Image` whose dockerfile starts with `"FROM base"` and whose `base_images`
//! map points at the previous layer (`modal/image.py:1717`, `:1019`,
//! `:2393`). Each layer is a separate `ImageGetOrCreate` round trip.
//!
//! [`ImageSpec`] collapses that chain into a **single** layer: the commands are
//! concatenated in the same order the Python client would have produced them,
//! so the resulting Dockerfile is byte-for-byte what `docker build` would see,
//! but it costs one RPC instead of four. The only observable difference is
//! Modal's build cache granularity.

use std::collections::BTreeMap;

use base64::Engine as _;

use crate::pb::modal_client as pb;
use crate::shim::SHIM_PATH;

/// The image-builder version amicu asks Modal to build with.
///
/// `modal/image.py:70` lists `"2023.12" | "2024.04" | "2024.10" | "2025.06" |
/// "PREVIEW"`; the Python client normally reads it from the *environment*
/// (`modal/image.py:565`), which would cost an extra environment lookup. amicu
/// pins the newest dated version instead, which matters for one behaviour: at
/// `> "2024.10"` the builder stops baking Modal's own client requirements into
/// the image (`modal/image.py:1874-1885`) — amicu does not want them there,
/// because it mounts them at runtime instead (see
/// [`crate::MODAL_CLIENT_MOUNT_NAME`]).
pub const IMAGE_BUILDER_VERSION: &str = "2025.06";

/// A one-layer container image: a registry base plus pip packages and env vars.
///
/// The generated Python shim is *not* part of this struct: `deploy_server`
/// renders it from the [`crate::ServerSpec`]'s `argv`/`port` and passes it to
/// [`ImageSpec::dockerfile_commands`], so there is exactly one source of truth
/// for the command the container runs.
///
/// `Debug` redacts [`ImageSpec::env`] values (an `ENV` entry is a plausible
/// place for a token even though [`crate::ServerSpec::secret_env`] is the
/// intended one).
#[derive(Clone)]
pub struct ImageSpec {
    /// Fully qualified registry reference, e.g. `python:3.12-slim` or
    /// `vllm/vllm-openai:v0.29.0`. **Must already contain Python 3.10-3.13 and
    /// `pip` on `PATH`**: amicu does not use Modal's `add_python=` path, which
    /// needs a `context_mount_id` pointing at Modal's globally deployed
    /// python-standalone Mount (`modal/image.py:1930-1938`) — extra RPC
    /// surface, and both images amicu targets (the official `vllm/vllm-openai`
    /// images and `python:*-slim`) ship Python already.
    pub base_registry_image: String,
    /// Extra pip packages. Modal's own client package is **not** installed
    /// here — it is mounted at runtime from Modal's global client mount, so
    /// nothing in this image can dependency-conflict with it.
    pub pip_packages: Vec<String>,
    /// `ENV` entries. Ordered (`BTreeMap`) so the Dockerfile — and therefore
    /// Modal's build cache key — is deterministic.
    pub env: BTreeMap<String, String>,
}

impl std::fmt::Debug for ImageSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImageSpec")
            .field("base_registry_image", &self.base_registry_image)
            .field("pip_packages", &self.pip_packages)
            // Keys only: an ENV value can carry a token.
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl ImageSpec {
    /// The `dockerfile_commands` list sent in the `Image` message, with
    /// `shim_source` written to [`SHIM_PATH`].
    ///
    /// Mirrors, in order, what the Python client emits for
    /// `Image.from_registry(base).entrypoint([]).pip_install(...).env(...)`:
    ///
    /// * `FROM <base>` — `modal/image.py:1888` (and nothing else from
    ///   `_registry_setup_commands`, because [`IMAGE_BUILDER_VERSION`] is
    ///   `> "2024.10"`: `modal/image.py:1874-1885`).
    /// * `ENTRYPOINT []` — `modal/image.py:1717`. Clears the base image's
    ///   entrypoint so Modal's container runtime, not the image's own launcher
    ///   (`vllm/vllm-openai` ships a `vllm serve` entrypoint), is `PID 1`.
    /// * `RUN python -m pip install <packages>` — `modal/image.py:1019`,
    ///   including its `shlex.join(sorted(pkgs))` normalisation. Omitted
    ///   entirely when there are no packages, matching `pip_install`'s early
    ///   return (`modal/image.py:1007-1008`).
    /// * `ENV <k>=<shell-quoted v>` — `modal/image.py:2393`.
    /// * the shim write, which has no Python-client equivalent: a plain `RUN`
    ///   like `Image.run_commands` emits (`modal/image.py:1754`). The source is
    ///   base64-encoded so that no part of it — quotes, newlines, `$`,
    ///   backticks — is ever interpreted by the build shell.
    #[must_use]
    pub fn dockerfile_commands(&self, shim_source: &str) -> Vec<String> {
        let mut commands = vec![
            format!("FROM {}", self.base_registry_image),
            "ENTRYPOINT []".to_owned(),
        ];

        if !self.pip_packages.is_empty() {
            let mut packages: Vec<&str> = self.pip_packages.iter().map(String::as_str).collect();
            packages.sort_unstable();
            let joined = packages
                .into_iter()
                .map(shell_quote)
                .collect::<Vec<_>>()
                .join(" ");
            commands.push(format!("RUN python -m pip install {joined}"));
        }

        for (key, value) in &self.env {
            commands.push(format!("ENV {key}={}", shell_quote(value)));
        }

        let encoded = base64::engine::general_purpose::STANDARD.encode(shim_source.as_bytes());
        commands.push(format!(
            "RUN printf '%s' '{encoded}' | base64 -d > {SHIM_PATH}"
        ));

        commands
    }

    /// The `Image` message for `ImageGetOrCreate`.
    ///
    /// `base_images` is empty because this is a first (`FROM <registry tag>`)
    /// layer rather than a `FROM base` extension — compare
    /// `modal/image.py:582-588`, which fills it from the `base_images` dict
    /// that only the extension helpers populate.
    pub(crate) fn to_proto(&self, shim_source: &str) -> pb::Image {
        pb::Image {
            dockerfile_commands: self.dockerfile_commands(shim_source),
            ..pb::Image::default()
        }
    }
}

/// POSIX-shell quoting, equivalent to Python's `shlex.quote`, which is what
/// `modal/image.py` uses for pip package names (`:1018`) and `ENV` values
/// (`:2393`).
fn shell_quote(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@%+=:,./-_".contains(c));
    if safe {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> ImageSpec {
        ImageSpec {
            base_registry_image: "python:3.12-slim".to_owned(),
            pip_packages: vec!["vllm==0.11.0".to_owned()],
            env: BTreeMap::from([
                ("HF_XET_HIGH_PERFORMANCE".to_owned(), "1".to_owned()),
                ("VLLM_ARGS".to_owned(), "a b".to_owned()),
            ]),
        }
    }

    #[test]
    fn emits_exactly_the_expected_dockerfile() {
        let commands = spec().dockerfile_commands("class Server:\n    pass\n");
        assert_eq!(
            commands,
            vec![
                "FROM python:3.12-slim",
                "ENTRYPOINT []",
                // sorted and shlex-quoted; no `modal` package — it is mounted
                // at runtime, not installed here.
                "RUN python -m pip install vllm==0.11.0",
                "ENV HF_XET_HIGH_PERFORMANCE=1",
                "ENV VLLM_ARGS='a b'",
                "RUN printf '%s' 'Y2xhc3MgU2VydmVyOgogICAgcGFzcwo=' \
                 | base64 -d > /root/amicu_server.py",
            ]
        );
    }

    #[test]
    fn omits_the_pip_layer_when_there_is_nothing_to_install() {
        let mut spec = spec();
        spec.pip_packages.clear();
        spec.env.clear();
        let commands = spec.dockerfile_commands("x = 1\n");
        assert_eq!(commands.len(), 3, "{commands:?}");
        assert!(!commands.iter().any(|c| c.contains("pip install")));
    }

    #[test]
    fn base64_round_trips_a_shim_full_of_shell_metacharacters() {
        let shim = "ARGV = json.loads(\"[\\\"$(rm -rf /)\\\", \\\"`x`\\\"]\")\n";
        let run = spec()
            .dockerfile_commands(shim)
            .into_iter()
            .find(|c| c.starts_with("RUN printf"))
            .expect("shim RUN command");

        let encoded = run
            .trim_start_matches("RUN printf '%s' '")
            .split('\'')
            .next()
            .expect("encoded payload");
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("valid base64");
        assert_eq!(String::from_utf8(decoded).expect("utf-8"), shim);
        // Nothing from the source leaked into the shell command verbatim.
        assert!(!run.contains("rm -rf"));
    }

    #[test]
    fn image_message_has_no_base_images_and_carries_the_commands() {
        let spec = spec();
        let image = spec.to_proto("x = 1\n");
        assert_eq!(
            image.base_images,
            Vec::<crate::pb::modal_client::BaseImage>::new()
        );
        assert_eq!(
            image.context_files,
            Vec::<crate::pb::modal_client::ImageContextFile>::new()
        );
        assert_eq!(
            image.dockerfile_commands,
            spec.dockerfile_commands("x = 1\n")
        );
    }

    #[test]
    fn debug_never_reveals_env_values() {
        let rendered = format!("{:?}", spec());
        assert!(rendered.contains("VLLM_ARGS"), "keys stay visible");
        assert!(!rendered.contains("a b"), "env value leaked into Debug");
    }

    #[test]
    fn shell_quote_matches_shlex_quote_semantics() {
        assert_eq!(shell_quote("vllm==0.11.0"), "vllm==0.11.0");
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$X"), "'$X'");
    }
}
