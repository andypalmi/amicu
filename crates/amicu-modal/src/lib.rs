//! A minimal tonic/rustls client for Modal's (modal.com) control-plane
//! gRPC API, and the deploy path that puts an autoscaling HTTP server on it.
//!
//! # What this crate does
//!
//! [`ModalClient::deploy_server`] reproduces what `modal deploy` does for the
//! class-based web server in Modal's own `vllm_inference` example, without
//! Python on the client side. Given a [`ServerSpec`] it issues, in order:
//!
//! 1. `AppGetByDeploymentName` — reuse the app if the name is already
//!    deployed ([`ModalClient::find_app`]).
//! 2. `AppCreate` (`APP_STATE_INITIALIZING`) — only when it is not.
//! 3. `ImageGetOrCreate` with a one-layer Dockerfile ([`ImageSpec`]), then
//!    `ImageJoinStreaming` until the build finishes, surfacing build log
//!    lines through a callback.
//! 4. `VolumeGetOrCreate` per entry in [`ServerSpec::volumes`].
//! 5. `SecretGetOrCreate` (anonymous, app-owned) — only when
//!    [`ServerSpec::secret_env`] is non-empty.
//! 6. `FunctionPrecreate` to reserve the Function id, then `FunctionCreate`
//!    with `is_server = true`.
//! 7. `AppPublish` (`APP_STATE_DEPLOYED`).
//!
//! The public URL comes back on `FunctionCreate` as the Function's
//! `flash_service_urls`; `AppPublish` returns only the dashboard page.
//! [`ModalClient::stop_app`] tears the deployment down (`AppStop`).
//!
//! # Where the Python goes
//!
//! Modal's container runtime *is* Python: it imports a module by name and
//! looks up a class in it. amicu neither ships nor runs Python —
//! [`render_shim`] renders a ~15-line class as a Rust string and [`ImageSpec`]
//! bakes it into the image (base64-encoded inside a `RUN`), where
//! `@modal.enter` starts the real server process with
//! `subprocess.Popen(argv)` and `@modal.exit` stops it. `argv` is embedded as
//! a JSON literal and never touches a shell.
//!
//! # Two hypotheses this crate cannot verify offline
//!
//! Modal's proto header says direct gRPC use is unsupported, and no
//! non-Python client has ever *defined* a Function (Modal's own Go/JS SDKs
//! only invoke). Two assumptions are therefore load-bearing and provable only
//! against a real account, which is what `tests/live_deploy.rs` exists for:
//!
//! **H1 — a module baked into the image is importable with `mount_ids = []`.**
//! The Python client always uploads two Mounts: its own package and the
//! user's source file (`_functions.py:730-733`). amicu sends neither: the
//! module is written into the image at `/root/amicu_server.py` and `modal`
//! itself is `pip install`ed there, with `mount_client_dependencies = false`
//! so the worker expects the client libraries in the image
//! (`api.proto:1633-1635`, the pre-2024.10 arrangement). This relies on
//! `/root` — the container's working directory — being on `sys.path` when
//! `importlib.import_module("amicu_server")` runs
//! (`_runtime/user_code_imports.py:517`).
//! *Fallback if it fails:* implement the mount upload path (`MountPutFile` /
//! `MountGetOrCreate`) and pass real `mount_ids`. That is strictly more work,
//! not a protocol blocker.
//!
//! **H2 — the `Function` field set for a class-based server is accepted.**
//! `crate::deploy` cites a Python source line for every non-obvious field, but
//! server-side validation may require something the client sets implicitly.
//! The likeliest surprises are the empty `method_definitions` map (correct per
//! `_partial_function.py`: `@modal.enter`/`@modal.exit` are lifecycle flags,
//! not interface flags, so a server class contributes no method definitions)
//! and the absent app heartbeat (`modal deploy` runs an `AppHeartbeat` loop
//! while the app is INITIALIZING; a short deploy should not need it).
//! *Fallback if it fails:* the gRPC status names the offending field, and the
//! fix is a field change here — no architectural change.
//!
//! `deploy_cpu_http_server` in `tests/live_deploy.rs` settles both for a few
//! cents on CPU; `deploy_vllm_smoke` then repeats it on a GPU.
//!
//! # Packaging note
//!
//! Only `amicu-cli` depends on this crate (via the `amicu modal auth-check`
//! command) — `amicu-api`/`amicu-server` do not, so the `amicu-server` binary
//! that actually ships in the Docker runtime image never links it. The Docker
//! *build* is not fully isolated from it, though: `Dockerfile`'s builder stage
//! runs `cargo chef cook --workspace`, which does compile this crate's
//! build-dependency graph (`protoc-bin-vendored`, `tonic-prost-build`) as part
//! of that caching layer, even though the final `cargo build -p amicu-server`
//! step never links the crate into the shipped binary.

mod client;
mod credentials;
mod deploy;
mod error;
mod image;
mod interceptor;
mod pb;
mod shim;

pub use client::{DEFAULT_ENDPOINT, ENDPOINT_ENV_VAR, Hello, ModalClient};
pub use credentials::{Credentials, MissingCredential};
pub use deploy::{Deployment, ServerSpec};
pub use error::ModalError;
pub use image::{IMAGE_BUILDER_VERSION, ImageSpec, MODAL_PACKAGE_SPEC};
pub use shim::{SHIM_CLASS_NAME, SHIM_MODULE_NAME, SHIM_PATH, render_shim};
