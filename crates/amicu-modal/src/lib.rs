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
//! 3. `AppHeartbeat` every 15 s, in a background task, for the whole of the
//!    rest of the call (`runner.py:550-555`, `config.py:241`). An app left
//!    INITIALIZING without heartbeats is reaped server-side, which would
//!    abort a long image build.
//! 4. `MountGetOrCreate` for Modal's globally published client-package Mount
//!    ([`MODAL_CLIENT_MOUNT_NAME`]).
//! 5. `ImageGetOrCreate` with a one-layer Dockerfile ([`ImageSpec`]), then
//!    `ImageJoinStreaming` until the build finishes, surfacing build log
//!    lines through a callback.
//! 6. `VolumeGetOrCreate` per entry in [`ServerSpec::volumes`].
//! 7. `SecretGetOrCreate` (anonymous, app-owned) — only when
//!    [`ServerSpec::secret_env`] is non-empty.
//! 8. `FunctionPrecreate` to reserve the Function id, then `FunctionCreate`
//!    with `is_server = true`.
//! 9. `AppPublish` (`APP_STATE_DEPLOYED`).
//!
//! Any failure after step 2 sends `AppClientDisconnect` with
//! `APP_DISCONNECT_REASON_DEPLOYMENT_EXCEPTION` (`runner.py:576-578`), so a
//! half-built app does not sit in INITIALIZING forever.
//!
//! The public URL comes back on `FunctionCreate` as the Function's
//! `flash_service_urls`; `AppPublish` returns only the dashboard page.
//! [`ModalClient::stop_app`] tears the deployment down (`AppStop`).
//!
//! # Where the Python goes
//!
//! Modal's container runtime *is* Python: it imports a module by name and
//! looks up a class in it. amicu neither ships nor runs Python —
//! [`render_shim`] renders a ~25-line class as a Rust string from the spec's
//! `argv`/`port`, and [`ImageSpec`] bakes it into the image (base64-encoded
//! inside a `RUN`). `@modal.enter` starts the real server process with
//! `subprocess.Popen(argv)`, waits for it to accept connections on `port`, and
//! raises if it exits first; `@modal.exit` stops it. `argv` is embedded as a
//! JSON literal and never touches a shell.
//!
//! The `modal` package itself is **not** installed into the image: it arrives
//! at container start from Modal's global client mount
//! ([`MODAL_CLIENT_MOUNT_NAME`]), with `Function.mount_client_dependencies =
//! true` — the same arrangement the Python client uses for every image builder
//! newer than 2024.10 (`_functions.py:964-966`,
//! `modal_proto/api.proto:1544-1546`). That keeps Modal's own pins (`click`,
//! `protobuf`, `grpclib`) out of an image like `vllm/vllm-openai`, where they
//! would risk a dependency conflict.
//!
//! # Two hypotheses this crate cannot verify offline
//!
//! Modal's proto header says direct gRPC use is unsupported, and no
//! non-Python client has ever *defined* a Function (Modal's own Go/JS SDKs
//! only invoke). Two assumptions are therefore load-bearing and provable only
//! against a real account, which is what `tests/live_deploy.rs` exists for:
//!
//! **H1 — the shim module baked into the image is importable.** The Python
//! client uploads the user's source file as a second Mount
//! (`_functions.py:730-733`); amicu writes it into the image instead, at
//! `/root/amicu_server.py`, and sends only Modal's client mount in
//! `mount_ids`. This relies on `/root` — the container's working directory —
//! being on `sys.path` when `importlib.import_module("amicu_server")` runs
//! (`_runtime/user_code_imports.py:517`). Mounting Modal's own package
//! narrows the hypothesis to exactly this one file.
//! *Fallback if it fails:* upload the shim as a Mount too (`MountPutFile` +
//! `MountGetOrCreate` with `files`, `mount.py:571-611`) and add its id to
//! `mount_ids`. More work, not a protocol blocker.
//!
//! **H2 — the `Function` field set for a class-based server is accepted.**
//! `crate::deploy` cites a Python source line for every non-obvious field, but
//! server-side validation may require something the client sets implicitly.
//! The likeliest surprise is the empty `method_definitions` map — correct per
//! `_partial_function.py`, where `@modal.enter`/`@modal.exit` are lifecycle
//! flags rather than interface flags, so a server class contributes no method
//! definitions at all.
//! *Fallback if it fails:* the gRPC status names the offending field, and the
//! fix is a field change here — no architectural change.
//!
//! `deploy_cpu_http_server` in `tests/live_deploy.rs` settles both for a few
//! cents on CPU; `deploy_vllm_smoke` then repeats it on a GPU.
//!
//! # Reading a deployment back: `crate::ops`
//!
//! [`ModalClient::stream_logs`] (`AppGetLogs`, mirroring
//! `get_app_logs_loop`, `modal/_output.py:543-675`),
//! [`ModalClient::app_status`] (`AppList` + `AppGetLayout` +
//! `FunctionGetCurrentStats`), [`ModalClient::list_apps`] (`AppList`), and
//! [`ModalClient::health`] (a plain HTTP GET, no gRPC) read a deployment
//! back; see `crate::ops` for the per-RPC citations.
//!
//! # Calling a deployed server (proxy auth)
//!
//! Modal Servers require proxy auth by default, so an unauthenticated request
//! to the `*.modal.direct` URL gets a `401` from Modal's proxy. Create a Proxy
//! Auth Token (dashboard: Settings -> Proxy Auth Tokens) and export
//! `MODAL_PROXY_TOKEN_ID` (`wk-...`) and `MODAL_PROXY_TOKEN_SECRET` (`ws-...`);
//! every probe in this crate sends them as `Modal-Key` / `Modal-Secret`
//! ([`proxy_auth`]). The secret is never logged. By hand:
//! `curl -H "Modal-Key: $MODAL_PROXY_TOKEN_ID" -H "Modal-Secret: $MODAL_PROXY_TOKEN_SECRET" "$URL/v1/models"`.
//!
//! # Packaging note
//!
//! Only `amicu-cli` depends on this crate (via the `amicu modal` commands, and — since [`supervisor`] — `amicu apply`'s dispatch for a
//! `provider = "modal"` vLLM deployment) — `amicu-api`/`amicu-server` do not,
//! so the `amicu-server` binary that actually ships in the Docker runtime
//! image never links it. The Docker *build* is not fully isolated from it,
//! though: `Dockerfile`'s builder stage runs `cargo chef cook --workspace`,
//! which does compile this crate's build-dependency graph
//! (`protoc-bin-vendored`, `tonic-prost-build`) as part of that caching
//! layer, even though the final `cargo build -p amicu-server` step never
//! links the crate into the shipped binary.
//!
//! [`supervisor`] adds `amicu-llamacpp`/`amicu-runtime`/`amicu-core`/`amicu-providers` as
//! dependencies of this crate (for the `ProcessSupervisor` trait, the
//! plan/handle types it exchanges, and the Modal proxy-region mapping). None of those crates depends back on
//! `amicu-modal`, so this stays a one-way edge - `amicu-engine` and
//! `amicu-vllm` still do not know Modal exists.

mod client;
mod credentials;
mod deploy;
mod error;
mod image;
mod interceptor;
mod ops;
mod pb;
pub mod proxy_auth;
mod shim;
pub mod supervisor;

pub use client::{DEFAULT_ENDPOINT, ENDPOINT_ENV_VAR, Hello, ModalClient};
pub use credentials::{Credentials, MissingCredential};
pub use deploy::{Deployment, MODAL_CLIENT_MOUNT_NAME, ServerSpec};
pub use error::ModalError;
pub use image::{IMAGE_BUILDER_VERSION, ImageSpec};
pub use ops::{AppStatus, AppSummary, FunctionStatus, HealthStatus, LogFd, LogLine};
pub use shim::{SHIM_CLASS_NAME, SHIM_MODULE_NAME, SHIM_PATH, render_shim};
pub use supervisor::{DEFAULT_VLLM_IMAGE, HF_TOKEN_ENV_VAR, ModalSupervisor, VLLM_IMAGE_ENV_VAR};
