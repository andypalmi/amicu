//! Deploying an autoscaling HTTP server to Modal.
//!
//! Every field of the `Function` message below is cited against
//! `modal-client` v1.3.1 (`proto/VENDORED.md`), because a non-Python client
//! has no way to test-drive the server's validation offline. Paths are
//! relative to the upstream checkout at that tag.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use crate::client::ModalClient;
use crate::error::ModalError;
use crate::image::{IMAGE_BUILDER_VERSION, ImageSpec};
use crate::pb::modal_client as pb;
use crate::shim::{SHIM_CLASS_NAME, SHIM_MODULE_NAME, render_shim};

/// Modal's proxy regions for the server's public endpoint.
///
/// `app.py:1216` defaults `proxy_regions` to `["us-east"]` and
/// `_server.py:34-35` rejects an empty list, so this is not optional. Making
/// it configurable is a follow-up.
const PROXY_REGIONS: [&str; 1] = ["us-east"];

/// The deployment name of Modal's globally published client-package Mount.
///
/// `mount.py:63-70` builds it as `f"modal-client-mount-{version}"` and
/// `_get_client_mount` (`mount.py:729-734`) looks it up with
/// `Mount.from_name(..., namespace=DEPLOYMENT_NAMESPACE_GLOBAL)`. Mounting it
/// is how the `modal` package gets into the container, so the image never has
/// to `pip install modal` — which matters because Modal's client pins
/// `click`, `protobuf` and `grpclib` ranges that can conflict with what a
/// model-server image (notably `vllm/vllm-openai`) already has installed.
///
/// The version must match `proto/VENDORED.md`.
pub const MODAL_CLIENT_MOUNT_NAME: &str = "modal-client-mount-1.3.1";

/// How often the app heartbeat fires while a deploy is in flight
/// (`config.py:241`, `"heartbeat_interval": _Setting(15, float)`).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Consecutive `ImageJoinStreaming` errors tolerated before giving up
/// (`image.py:396-403`).
const IMAGE_JOIN_MAX_RETRIES: u32 = 3;

/// How long `ImageJoinStreaming` may report nothing at all before the build is
/// declared wedged.
///
/// Modal's own client has no such limit (`image.py:394-404` loops until a
/// result arrives), and silence is legitimate: a build queued behind others
/// produces no log lines until it starts. This ceiling exists only so a stream
/// that will never speak again cannot spin indefinitely, so it is set far
/// above any plausible queue wait.
const IMAGE_BUILD_SILENCE_LIMIT: Duration = Duration::from_mins(30);

/// What [`ModalClient::deploy_server`] produced.
#[derive(Debug, Clone)]
pub struct Deployment {
    /// The Modal app id (`ap-…`). Pass to [`ModalClient::stop_app`].
    pub app_id: String,
    /// The server Function's id (`fu-…`).
    pub function_id: String,
    /// The public HTTPS endpoint, e.g.
    /// `https://<workspace>--<app>-server.us-east.modal.direct`.
    pub url: String,
}

/// A server to deploy: one container image running one long-lived process
/// behind Modal's HTTP proxy.
///
/// `Debug` redacts [`ServerSpec::secret_env`] values (and, via
/// [`ImageSpec`]'s own `Debug`, `ENV` values).
#[derive(Clone)]
pub struct ServerSpec {
    /// Deployment name of the Modal app. Reused across deploys: an existing
    /// app with this name is updated in place.
    pub app_name: String,
    /// GPU request in Modal's string form — `"H100"`, `"H100:2"`, `"L4"` —
    /// or `None` for a CPU-only container.
    pub gpu: Option<String>,
    /// Optional region constraint, e.g. `"us-east"`.
    pub region: Option<String>,
    /// The port the process in [`ServerSpec::argv`] listens on. Baked into the
    /// generated shim's readiness probe as well as `http_config.port`.
    pub port: u16,
    /// The command the container runs, verbatim; never shell-interpreted.
    /// `deploy_server` renders the container-side shim from this, so it is the
    /// single source of truth for what runs.
    pub argv: Vec<String>,
    /// The image to build and run.
    pub image: ImageSpec,
    /// `(volume name, mount path)` pairs, created if missing.
    pub volumes: Vec<(String, String)>,
    /// Environment variables injected via an app-scoped Modal Secret. Values
    /// are never logged, printed, or included in errors.
    pub secret_env: BTreeMap<String, String>,
    /// Idle seconds before a replica is scaled down.
    pub scaledown_window_secs: u32,
    /// Seconds Modal waits for the port to accept connections. Must be > 0
    /// (`_server.py:30-31`).
    pub startup_timeout_secs: u32,
    /// Maximum lifetime of a single container, in seconds.
    pub timeout_secs: u32,
    /// Requests a single replica should handle concurrently.
    pub target_concurrency: u32,
    /// Replicas to keep warm; `0` means scale to zero.
    pub min_containers: u32,
    /// Replica ceiling, or `None` for Modal's default.
    pub max_containers: Option<u32>,
}

impl std::fmt::Debug for ServerSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerSpec")
            .field("app_name", &self.app_name)
            .field("gpu", &self.gpu)
            .field("region", &self.region)
            .field("port", &self.port)
            .field("argv", &self.argv)
            .field("image", &self.image)
            .field("volumes", &self.volumes)
            // Keys only — a secret value must never reach a log line.
            .field("secret_env", &self.secret_env.keys().collect::<Vec<_>>())
            .field("scaledown_window_secs", &self.scaledown_window_secs)
            .field("startup_timeout_secs", &self.startup_timeout_secs)
            .field("timeout_secs", &self.timeout_secs)
            .field("target_concurrency", &self.target_concurrency)
            .field("min_containers", &self.min_containers)
            .field("max_containers", &self.max_containers)
            .finish()
    }
}

/// Parses Modal's GPU string (`"H100"`, `"H100:2"`) into a `GPUConfig`,
/// mirroring `modal/gpu.py:201-222`: split once on `:`, parse the suffix as
/// the count, upper-case the type, default the count to 1.
fn parse_gpu_config(spec: &str) -> Result<pb::GpuConfig, ModalError> {
    let (gpu_type, count) = match spec.split_once(':') {
        Some((gpu_type, count)) => {
            let count = count
                .parse::<u32>()
                .map_err(|_| ModalError::InvalidGpu { spec: spec.into() })?;
            (gpu_type, count)
        }
        None => (spec, 1),
    };
    if gpu_type.is_empty() || count == 0 {
        return Err(ModalError::InvalidGpu { spec: spec.into() });
    }
    Ok(pb::GpuConfig {
        gpu_type: gpu_type.to_uppercase(),
        count,
        ..pb::GpuConfig::default()
    })
}

impl ServerSpec {
    /// Client-side validation, mirroring `validate_http_server_config`
    /// (`_server.py:22-35`). `port` is a `u16` so the upper bound is already
    /// enforced by the type; the lower bound and `startup_timeout` are not.
    fn validate(&self) -> Result<(), ModalError> {
        if self.port == 0 {
            return Err(ModalError::InvalidSpec {
                field: "port",
                detail: "must be between 1 and 65535 (_server.py:28-29)".to_owned(),
            });
        }
        if self.startup_timeout_secs == 0 {
            return Err(ModalError::InvalidSpec {
                field: "startup_timeout_secs",
                detail: "must be positive (_server.py:30-31)".to_owned(),
            });
        }
        if self.argv.is_empty() {
            return Err(ModalError::InvalidSpec {
                field: "argv",
                detail: "the container needs a command to run".to_owned(),
            });
        }
        Ok(())
    }

    /// Builds the `Function` message, mirroring what
    /// `app._experimental_server(...)` (`app.py:1193-1338`) feeds to
    /// `_Function.from_local` (`_functions.py:660-1180`) for a class-based
    /// web server.
    fn function_definition(
        &self,
        image_id: &str,
        client_mount_id: &str,
        secret_ids: Vec<String>,
        volume_mounts: Vec<pb::VolumeMount>,
    ) -> Result<pb::Function, ModalError> {
        let gpu_config = self
            .gpu
            .as_deref()
            .map(parse_gpu_config)
            .transpose()?
            // `parse_gpu_config(None)` returns an empty GPUConfig, and
            // `convert_fn_config_to_resources_config` always sets `resources`
            // (`_functions.py:1121-1125`, `_resources.py:10-18`).
            .unwrap_or_default();

        // `_deps(only_explicit_mounts=True)` (`_functions.py:870-889`, called
        // at `:984`): secrets, then proxy, then image, then network file
        // systems, then volumes. Mounts are deliberately *excluded* — the
        // `only_explicit_mounts` flag skips `all_mounts` (`:872-873`) — so the
        // client mount below is in `mount_ids` but not here.
        let mut object_dependencies: Vec<pb::ObjectDependency> = secret_ids
            .iter()
            .map(|id| pb::ObjectDependency {
                object_id: id.clone(),
            })
            .collect();
        object_dependencies.push(pb::ObjectDependency {
            object_id: image_id.to_owned(),
        });
        object_dependencies.extend(volume_mounts.iter().map(|m| pb::ObjectDependency {
            object_id: m.volume_id.clone(),
        }));

        Ok(pb::Function {
            // `FunctionInfo.module_name` is the *file stem* of the module the
            // class lives in (`_utils/function_utils.py:210`); ours is baked
            // into the image at `/root/amicu_server.py`.
            module_name: SHIM_MODULE_NAME.to_owned(),
            // `app.py:1297` passes `name_override=user_cls.__name__`, which
            // becomes `function_name` (`_utils/function_utils.py:172`). The
            // container splits this to find the class and explicitly allows
            // the bare `ClassName` form for servers
            // (`_runtime/user_code_imports.py:524-532`).
            function_name: SHIM_CLASS_NAME.to_owned(),
            // `f"{user_cls.__name__}.*"` for a class service function
            // (`_utils/function_utils.py:165`).
            implementation_name: format!("{SHIM_CLASS_NAME}.*"),
            // The Python client sends two mounts (`_functions.py:730-733`):
            // its own package and the user's source file. amicu bakes the
            // source into the image but still mounts Modal's client package —
            // that is what `mount_client_dependencies` below refers to, and it
            // keeps the image free of Modal's pip pins.
            mount_ids: vec![client_mount_id.to_owned()],
            image_id: image_id.to_owned(),
            secret_ids,
            object_dependencies,
            // Not serialized (no cloudpickle), so FILE
            // (`_utils/function_utils.py:141-145`); `function_serialized` and
            // `class_serialized` stay empty (`_functions.py:955-957`).
            definition_type: pb::function::DefinitionType::File as i32,
            // `get_function_type(is_generator=None)`
            // (`_utils/function_utils.py:116-117`).
            function_type: pb::function::FunctionType::Function as i32,
            // Empty but *set*. `method_definitions` is populated only from
            // `_PartialFunctionFlags.interface_flags()` — CALLABLE_INTERFACE
            // and WEB_INTERFACE (`_partial_function.py:38-39`, `:64-66`) —
            // which `@modal.enter`/`@modal.exit` are not: those are lifecycle
            // flags (`_partial_function.py:32-36`, `:56-61`). So a server class
            // whose only decorated methods are enter/exit yields `{}` here
            // (`_functions.py:838-866`), with `method_definitions_set=True`
            // unconditionally (`_functions.py:1007`).
            method_definitions: HashMap::new(),
            method_definitions_set: true,
            // `info.is_service_class()` is True when `raw_f is None`
            // (`_utils/function_utils.py:147-151`), which is the class case.
            is_class: true,
            // `bool(info.user_cls) and not info.is_service_class()`
            // (`_functions.py:1027`) → False for the service class itself.
            is_method: false,
            // `info.class_parameter_info()` with no annotated parameters
            // (`_utils/function_utils.py:292-301`), sent at
            // `_functions.py:1037`.
            class_parameter_info: Some(pb::ClassParameterInfo {
                format: pb::class_parameter_info::ParameterSerializationFormat::ParamSerializationFormatPickle
                    as i32,
                schema: Vec::new(),
            }),
            // `app.py:1327`.
            is_server: true,
            // `app.py:1264-1270`. Note `target_concurrency` is *not* part of
            // HTTPConfig in v1.3.1 even though the message has such a field:
            // the decorator routes it to `target_concurrent_inputs` below
            // (`app.py:1317`).
            http_config: Some(pb::HttpConfig {
                port: u32::from(self.port),
                proxy_regions: PROXY_REGIONS.iter().map(|r| (*r).to_owned()).collect(),
                startup_timeout: self.startup_timeout_secs,
                // `app.py:1215` default.
                exit_grace_period: 0,
                // `app.py:1217` default.
                h2_enabled: false,
                target_concurrency: 0,
            }),
            // `_functions.py:1022` (`target_concurrent_inputs or 0`).
            target_concurrent_inputs: self.target_concurrency,
            // `_functions.py:784-789`.
            autoscaler_settings: Some(pb::AutoscalerSettings {
                min_containers: Some(self.min_containers),
                max_containers: self.max_containers,
                buffer_containers: None,
                scaleup_window: None,
                scaledown_window: Some(self.scaledown_window_secs),
            }),
            // Deprecated mirrors the client still sends alongside
            // `autoscaler_settings` (`_functions.py:1048-1052`).
            warm_pool_size: self.min_containers,
            concurrency_limit: self.max_containers.unwrap_or(0),
            task_idle_timeout_secs: self.scaledown_window_secs,
            // `_functions.py:1012`.
            timeout_secs: self.timeout_secs,
            // `startup_timeout or timeout_secs` (`_functions.py:1013`);
            // `validate` has already rejected a zero startup timeout.
            startup_timeout_secs: self.startup_timeout_secs,
            resources: Some(pb::Resources {
                gpu_config: Some(gpu_config),
                ..pb::Resources::default()
            }),
            // `region=` becomes a SchedulerPlacement with the region list
            // (`_functions.py:755-757`); unset when no region is requested.
            scheduler_placement: self.region.as_ref().map(|region| pb::SchedulerPlacement {
                regions: vec![region.clone()],
                nonpreemptible: false,
                ..pb::SchedulerPlacement::default()
            }),
            // `_functions.py:969-976`.
            volume_mounts,
            // `_functions.py:1019`.
            app_name: self.app_name.clone(),
            // True, matching the Python client for any image builder newer
            // than 2024.10 (`_functions.py:964-966`): Modal's client libraries
            // are mounted into the container at runtime and are "no longer
            // expected to exist in the image itself"
            // (`modal_proto/api.proto:1544-1546`). The mount that carries them
            // is `mount_ids[0]`.
            mount_client_dependencies: true,
            // Hard-coded True by the client (`_functions.py:1045`).
            experimental_concurrent_cancellations: true,
            ..pb::Function::default()
        })
    }
}

/// Keeps `AppHeartbeat` firing for the lifetime of a deploy.
///
/// `modal deploy` runs this loop for the whole of `_create_all_objects` +
/// `_publish_app` (`runner.py:550-555`); an app left INITIALIZING without
/// heartbeats is reaped server-side, which would abort a long image build.
/// Dropping the guard aborts the task.
struct HeartbeatGuard(tokio::task::JoinHandle<()>);

impl Drop for HeartbeatGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ModalClient {
    /// Looks up an existing deployed app id by name (`AppGetByDeploymentName`,
    /// `runner.py:102-123`). `Ok(None)` when no such deployment exists.
    pub async fn find_app(&mut self, name: &str) -> Result<Option<String>, ModalError> {
        let response = self
            .stub()
            .app_get_by_deployment_name(pb::AppGetByDeploymentNameRequest {
                name: name.to_owned(),
                environment_name: String::new(),
            })
            .await
            .map_err(|status| ModalError::from_status("AppGetByDeploymentName", status))?
            .into_inner();
        Ok((!response.app_id.is_empty()).then_some(response.app_id))
    }

    /// Stops a deployed app (`AppStop`), terminating its containers.
    pub async fn stop_app(&mut self, app_id: &str) -> Result<(), ModalError> {
        self.stub()
            .app_stop(pb::AppStopRequest {
                app_id: app_id.to_owned(),
                // `AppStopSource` has no libmodal variant; `modal app stop`
                // sends CLI, which is the closest honest match.
                source: pb::AppStopSource::Cli as i32,
            })
            .await
            .map_err(|status| ModalError::from_status("AppStop", status))?;
        Ok(())
    }

    /// Builds the image, creates the server Function, and publishes the app,
    /// returning its public URL.
    ///
    /// Issues, in the order `modal deploy` does (`runner.py:494-590`):
    /// `AppGetByDeploymentName` → `AppCreate` (only when absent) →
    /// [`AppHeartbeat` every 15s for the rest of the call] →
    /// `MountGetOrCreate` (Modal's global client mount) → `ImageGetOrCreate`
    /// (+ `ImageJoinStreaming` until built) → `VolumeGetOrCreate` per volume →
    /// `SecretGetOrCreate` (only when `secret_env` is non-empty) →
    /// `FunctionPrecreate` → `FunctionCreate` → `AppPublish`.
    ///
    /// On any failure after the app exists, `AppClientDisconnect` is sent with
    /// `APP_DISCONNECT_REASON_DEPLOYMENT_EXCEPTION` (`runner.py:576-578`) so
    /// the app is not left INITIALIZING forever.
    ///
    /// `on_build_log` receives image build output as Modal streams it.
    pub async fn deploy_server(
        &mut self,
        spec: &ServerSpec,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<Deployment, ModalError> {
        spec.validate()?;

        let app_id = match self.find_app(&spec.app_name).await? {
            Some(app_id) => app_id,
            None => self.create_app(&spec.app_name).await?,
        };

        let _heartbeat = self.spawn_heartbeat(&app_id);
        // `Box::pin`: the chain of tonic calls below adds up to a few tens of
        // KiB of future (clippy::large_futures).
        match Box::pin(self.deploy_into_app(&app_id, spec, on_build_log)).await {
            Ok(deployment) => Ok(deployment),
            Err(error) => {
                // Best effort: the original error is what the caller needs, so
                // a failure to disconnect only replaces it if there is nothing
                // else to report.
                let _ = self.disconnect_after_failure(&app_id, &error).await;
                Err(error)
            }
        }
    }

    /// Everything between app creation and publish. Split out so
    /// [`ModalClient::deploy_server`] has exactly one error funnel.
    async fn deploy_into_app(
        &mut self,
        app_id: &str,
        spec: &ServerSpec,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<Deployment, ModalError> {
        let client_mount_id = self.get_client_mount().await?;

        let shim_source = render_shim(&spec.argv, spec.port);
        let image_id = self
            .build_image(app_id, &spec.image, &shim_source, on_build_log)
            .await?;

        let mut volume_mounts = Vec::with_capacity(spec.volumes.len());
        for (name, mount_path) in &spec.volumes {
            volume_mounts.push(pb::VolumeMount {
                volume_id: self.get_or_create_volume(name).await?,
                mount_path: mount_path.clone(),
                // `_functions.py:972`.
                allow_background_commits: true,
                read_only: false,
            });
        }

        let secret_ids = if spec.secret_env.is_empty() {
            Vec::new()
        } else {
            vec![self.create_app_secret(app_id, &spec.secret_env).await?]
        };

        let function =
            spec.function_definition(&image_id, &client_mount_id, secret_ids, volume_mounts)?;

        // Preassign the Function's id so `FunctionCreate` updates it in place
        // rather than creating a second one (`_functions.py:900-917`).
        let precreated = self
            .stub()
            .function_precreate(pb::FunctionPrecreateRequest {
                app_id: app_id.to_owned(),
                function_name: function.function_name.clone(),
                function_type: function.function_type,
                existing_function_id: String::new(),
                ..pb::FunctionPrecreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("FunctionPrecreate", status))?
            .into_inner();

        let created = self
            .stub()
            .function_create(pb::FunctionCreateRequest {
                app_id: app_id.to_owned(),
                function: Some(function),
                existing_function_id: precreated.function_id,
                ..pb::FunctionCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("FunctionCreate", status))?
            .into_inner();

        // For a server the endpoint comes back on the created Function as
        // `flash_service_urls` (`_utils/function_utils.py:663-670`), not as
        // `web_url`; `AppPublish` returns only the dashboard page URL
        // (`runner.py:216-218`).
        let url = created
            .function
            .as_ref()
            .and_then(|f| f.flash_service_urls.first())
            .cloned()
            .ok_or(ModalError::MissingServerUrl)?;
        let definition_id = created
            .handle_metadata
            .as_ref()
            .map(|m| m.definition_id.clone())
            .unwrap_or_default();

        // `runner.py:191-218`: function_ids maps *function name* to id, which
        // is how the container later finds itself
        // (`_container_entrypoint.py:304-306`). `class_ids` stays empty —
        // `app._experimental_server` registers only the service Function
        // (`app.py:1336`).
        self.stub()
            .app_publish(pb::AppPublishRequest {
                app_id: app_id.to_owned(),
                name: spec.app_name.clone(),
                app_state: pb::AppState::Deployed as i32,
                function_ids: HashMap::from([(
                    SHIM_CLASS_NAME.to_owned(),
                    created.function_id.clone(),
                )]),
                definition_ids: HashMap::from([(created.function_id.clone(), definition_id)]),
                ..pb::AppPublishRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("AppPublish", status))?;

        Ok(Deployment {
            app_id: app_id.to_owned(),
            function_id: created.function_id,
            url,
        })
    }

    /// Spawns the `AppHeartbeat` loop on a cloned stub (the generated tonic
    /// client is `Clone`, and the underlying `Channel` multiplexes).
    fn spawn_heartbeat(&mut self, app_id: &str) -> HeartbeatGuard {
        let mut stub = self.stub().clone();
        let request = pb::AppHeartbeatRequest {
            app_id: app_id.to_owned(),
        };
        HeartbeatGuard(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
            // The first tick is immediate; skip it, the app was just created.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                // A failed heartbeat is not fatal to the deploy: the RPC that
                // is actually in flight will surface the real problem.
                let _ = stub.app_heartbeat(request.clone()).await;
            }
        }))
    }

    /// `AppClientDisconnect` with `DEPLOYMENT_EXCEPTION`, which stops an app
    /// that is still INITIALIZING and is a no-op otherwise
    /// (`runner.py:576-578`, and the comment above it).
    async fn disconnect_after_failure(
        &mut self,
        app_id: &str,
        error: &ModalError,
    ) -> Result<(), ModalError> {
        // `_disconnect` truncates the exception text to 1000 chars
        // (`runner.py:231-232`). `ModalError`'s Display never contains a
        // token or a secret value (see `credentials.rs` and `ServerSpec`'s
        // `Debug`), so it is safe to send.
        let mut exception = error.to_string();
        exception.truncate(1000);
        self.stub()
            .app_client_disconnect(pb::AppClientDisconnectRequest {
                app_id: app_id.to_owned(),
                reason: pb::AppDisconnectReason::DeploymentException as i32,
                exception,
            })
            .await
            .map_err(|status| ModalError::from_status("AppClientDisconnect", status))?;
        Ok(())
    }

    /// `AppCreate` in the INITIALIZING state, as `modal deploy` does before
    /// publishing (`runner.py:83-90`).
    async fn create_app(&mut self, name: &str) -> Result<String, ModalError> {
        let response = self
            .stub()
            .app_create(pb::AppCreateRequest {
                description: name.to_owned(),
                environment_name: String::new(),
                app_state: pb::AppState::Initializing as i32,
                ..pb::AppCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("AppCreate", status))?
            .into_inner();
        Ok(response.app_id)
    }

    /// Resolves Modal's globally published client-package Mount by name —
    /// `Mount.from_name(..., namespace=GLOBAL)`'s loader, which sends a
    /// `MountGetOrCreateRequest` carrying only the name, namespace and
    /// environment and therefore performs a pure lookup
    /// (`mount.py:652-658`, reached from `_get_client_mount` at `:729-734`).
    async fn get_client_mount(&mut self) -> Result<String, ModalError> {
        let response = self
            .stub()
            .mount_get_or_create(pb::MountGetOrCreateRequest {
                deployment_name: MODAL_CLIENT_MOUNT_NAME.to_owned(),
                namespace: pb::DeploymentNamespace::Global as i32,
                environment_name: String::new(),
                ..pb::MountGetOrCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("MountGetOrCreate", status))?
            .into_inner();
        Ok(response.mount_id)
    }

    async fn get_or_create_volume(&mut self, name: &str) -> Result<String, ModalError> {
        let response = self
            .stub()
            .volume_get_or_create(pb::VolumeGetOrCreateRequest {
                deployment_name: name.to_owned(),
                environment_name: String::new(),
                // `volume.py:445-450` with `create_if_missing=True`.
                object_creation_type: pb::ObjectCreationType::CreateIfMissing as i32,
                ..pb::VolumeGetOrCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("VolumeGetOrCreate", status))?
            .into_inner();
        Ok(response.volume_id)
    }

    /// An anonymous, app-scoped Secret — what `Secret.from_dict` becomes when
    /// an app is being loaded (`secret.py:207-224`). The values travel over
    /// the TLS gRPC channel only and are never surfaced again.
    async fn create_app_secret(
        &mut self,
        app_id: &str,
        env: &BTreeMap<String, String>,
    ) -> Result<String, ModalError> {
        let response = self
            .stub()
            .secret_get_or_create(pb::SecretGetOrCreateRequest {
                object_creation_type: pb::ObjectCreationType::AnonymousOwnedByApp as i32,
                env_dict: env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                app_id: app_id.to_owned(),
                environment_name: String::new(),
                ..pb::SecretGetOrCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("SecretGetOrCreate", status))?
            .into_inner();
        Ok(response.secret_id)
    }

    /// `ImageGetOrCreate`, then `ImageJoinStreaming` until the build finishes,
    /// mirroring `image.py:654-668` and `_image_await_build_result`
    /// (`image.py:371-404`).
    async fn build_image(
        &mut self,
        app_id: &str,
        spec: &ImageSpec,
        shim_source: &str,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<String, ModalError> {
        let response = self
            .stub()
            .image_get_or_create(pb::ImageGetOrCreateRequest {
                app_id: app_id.to_owned(),
                image: Some(spec.to_proto(shim_source)),
                namespace: pb::DeploymentNamespace::Workspace as i32,
                builder_version: IMAGE_BUILDER_VERSION.to_owned(),
                ..pb::ImageGetOrCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("ImageGetOrCreate", status))?
            .into_inner();

        let image_id = response.image_id;
        // A non-zero status means the image is already built (`image.py:670`).
        let mut result = response.result.filter(|r| r.status != 0);

        let mut last_entry_id = String::new();
        let mut consecutive_failures = 0_u32;
        let mut silent_since = std::time::Instant::now();

        while result.is_none() {
            match self
                .join_image_build(&image_id, &mut last_entry_id, &mut result, on_build_log)
                .await
            {
                Ok(messages) => {
                    consecutive_failures = 0;
                    // Modal's server holds each `ImageJoinStreaming` call open
                    // for the requested timeout and then ends the stream, so
                    // empty rounds are normal — a build queued behind others
                    // can stay silent for a long time, and `image.py:394-404`
                    // never gives up on silence at all. amicu keeps only a
                    // generous ceiling, so a wedged stream cannot spin forever.
                    if messages == 0 && result.is_none() {
                        if silent_since.elapsed() >= IMAGE_BUILD_SILENCE_LIMIT {
                            return Err(ModalError::ImageBuild {
                                image_id,
                                detail: "Modal stopped reporting build progress".to_owned(),
                            });
                        }
                    } else {
                        silent_since = std::time::Instant::now();
                    }
                }
                // `image.py:396-403` tolerates a few transient stream errors
                // before giving up.
                Err(error) => {
                    consecutive_failures += 1;
                    if consecutive_failures >= IMAGE_JOIN_MAX_RETRIES {
                        return Err(error);
                    }
                }
            }
        }

        let result = result.expect("loop exits only once a result arrived");
        if result.status != pb::generic_result::GenericStatus::Success as i32 {
            return Err(ModalError::ImageBuild {
                image_id,
                // `result.exception` is Modal's build error text; it carries no
                // amicu-supplied secret (the shim is base64 in a RUN line and
                // env secrets are a separate Secret object, not image content).
                detail: if result.exception.is_empty() {
                    "see the build logs".to_owned()
                } else {
                    result.exception
                },
            });
        }
        Ok(image_id)
    }

    /// One `ImageJoinStreaming` round; returns how many messages arrived.
    /// Mirrors the `join()` closure at `image.py:376-392`.
    async fn join_image_build(
        &mut self,
        image_id: &str,
        last_entry_id: &mut String,
        result: &mut Option<pb::GenericResult>,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<usize, ModalError> {
        let mut stream = self
            .stub()
            .image_join_streaming(pb::ImageJoinStreamingRequest {
                image_id: image_id.to_owned(),
                timeout: 55.0,
                last_entry_id: last_entry_id.clone(),
                include_logs_for_finished: false,
            })
            .await
            .map_err(|status| ModalError::from_status("ImageJoinStreaming", status))?
            .into_inner();

        let mut messages = 0;
        while let Some(message) = stream
            .message()
            .await
            .map_err(|status| ModalError::from_status("ImageJoinStreaming", status))?
        {
            messages += 1;
            if !message.entry_id.is_empty() {
                last_entry_id.clone_from(&message.entry_id);
            }
            for log in &message.task_logs {
                // Progress-only entries carry a byte counter, not log text
                // (`image.py:384-389`); there is nothing to print.
                let is_progress = log
                    .task_progress
                    .as_ref()
                    .is_some_and(|p| p.pos != 0 || p.len != 0);
                if !is_progress && !log.data.is_empty() {
                    on_build_log(&log.data);
                }
            }
            if let Some(r) = message.result
                && r.status != 0
            {
                *result = Some(r);
            }
        }
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_spec() -> ServerSpec {
        ServerSpec {
            app_name: "amicu-test".to_owned(),
            gpu: Some("H100:2".to_owned()),
            region: Some("us-east".to_owned()),
            port: 8000,
            argv: ["vllm", "serve", "tiny"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            image: ImageSpec {
                base_registry_image: "vllm/vllm-openai:v0.29.0".to_owned(),
                pip_packages: Vec::new(),
                env: BTreeMap::new(),
            },
            volumes: vec![(
                "amicu-hf-cache".to_owned(),
                "/root/.cache/huggingface".to_owned(),
            )],
            secret_env: BTreeMap::from([("HF_TOKEN".to_owned(), "hf_supersecret".to_owned())]),
            scaledown_window_secs: 900,
            startup_timeout_secs: 600,
            timeout_secs: 3600,
            target_concurrency: 100,
            min_containers: 0,
            max_containers: Some(1),
        }
    }

    fn sample_function(spec: &ServerSpec) -> pb::Function {
        spec.function_definition(
            "im-123",
            "mo-client",
            vec!["st-9".to_owned()],
            vec![pb::VolumeMount {
                volume_id: "vo-1".to_owned(),
                mount_path: "/root/.cache/huggingface".to_owned(),
                allow_background_commits: true,
                read_only: false,
            }],
        )
        .expect("valid spec")
    }

    #[test]
    fn gpu_strings_map_to_type_and_count() {
        let one = parse_gpu_config("H100").expect("valid");
        assert_eq!(one.gpu_type, "H100");
        assert_eq!(one.count, 1);

        let two = parse_gpu_config("h100:2").expect("valid");
        assert_eq!(two.gpu_type, "H100", "the type is upper-cased");
        assert_eq!(two.count, 2);

        assert_eq!(
            parse_gpu_config("A100-80GB").expect("valid").gpu_type,
            "A100-80GB"
        );
    }

    #[test]
    fn invalid_gpu_strings_are_rejected() {
        for bad in ["H100:x", "H100:", "H100:0", ":2", "H100:-1"] {
            let Err(err) = parse_gpu_config(bad) else {
                panic!("expected {bad:?} to be rejected");
            };
            assert!(matches!(err, ModalError::InvalidGpu { .. }), "{bad}");
            assert!(err.to_string().contains(bad));
        }
    }

    #[test]
    fn a_zero_port_or_startup_timeout_or_empty_argv_is_rejected() {
        for (mutate, field) in [
            (
                Box::new(|s: &mut ServerSpec| s.port = 0) as Box<dyn Fn(&mut ServerSpec)>,
                "port",
            ),
            (
                Box::new(|s: &mut ServerSpec| s.startup_timeout_secs = 0),
                "startup_timeout_secs",
            ),
            (Box::new(|s: &mut ServerSpec| s.argv.clear()), "argv"),
        ] {
            let mut spec = sample_spec();
            mutate(&mut spec);
            let Err(err) = spec.validate() else {
                panic!("expected {field} to be rejected");
            };
            assert!(
                matches!(&err, ModalError::InvalidSpec { field: f, .. } if *f == field),
                "{err}"
            );
        }
        assert!(sample_spec().validate().is_ok());
    }

    #[test]
    fn function_definition_matches_the_python_client_shape() {
        let spec = sample_spec();
        let function = sample_function(&spec);

        assert_eq!(function.module_name, "amicu_server");
        assert_eq!(function.function_name, "Server");
        assert_eq!(function.implementation_name, "Server.*");
        assert_eq!(function.mount_ids, vec!["mo-client".to_owned()]);
        assert!(function.mount_client_dependencies);
        assert_eq!(function.image_id, "im-123");
        assert_eq!(function.secret_ids, vec!["st-9".to_owned()]);
        assert_eq!(
            function.definition_type,
            pb::function::DefinitionType::File as i32
        );
        assert_eq!(
            function.function_type,
            pb::function::FunctionType::Function as i32
        );
        assert!(function.is_class);
        assert!(!function.is_method);
        assert!(function.is_server);
        assert!(function.method_definitions.is_empty());
        assert!(function.method_definitions_set);
        assert!(function.experimental_concurrent_cancellations);
        assert_eq!(function.function_serialized, Vec::<u8>::new());
        assert_eq!(function.class_serialized, Vec::<u8>::new());
        assert_eq!(function.app_name, "amicu-test");
        assert_eq!(function.timeout_secs, 3600);
        assert_eq!(function.startup_timeout_secs, 600);
        assert_eq!(function.target_concurrent_inputs, 100);

        let http = function.http_config.expect("http_config");
        assert_eq!(http.port, 8000);
        assert_eq!(http.proxy_regions, vec!["us-east".to_owned()]);
        assert_eq!(http.startup_timeout, 600);
        assert_eq!(http.exit_grace_period, 0);
        assert!(!http.h2_enabled);

        let autoscaler = function.autoscaler_settings.expect("autoscaler_settings");
        assert_eq!(autoscaler.min_containers, Some(0));
        assert_eq!(autoscaler.max_containers, Some(1));
        assert_eq!(autoscaler.scaledown_window, Some(900));
        assert_eq!(function.warm_pool_size, 0);
        assert_eq!(function.concurrency_limit, 1);
        assert_eq!(function.task_idle_timeout_secs, 900);

        let gpu = function
            .resources
            .expect("resources")
            .gpu_config
            .expect("gpu_config");
        assert_eq!(gpu.gpu_type, "H100");
        assert_eq!(gpu.count, 2);

        assert_eq!(
            function
                .scheduler_placement
                .expect("scheduler_placement")
                .regions,
            vec!["us-east".to_owned()]
        );
        assert_eq!(function.volume_mounts.len(), 1);
        assert!(function.volume_mounts[0].allow_background_commits);
    }

    #[test]
    fn object_dependencies_cover_secrets_image_and_volumes_in_deps_order() {
        let ids: Vec<String> = sample_function(&sample_spec())
            .object_dependencies
            .into_iter()
            .map(|d| d.object_id)
            .collect();
        // `_deps(only_explicit_mounts=True)`: secrets, image, volumes — and
        // explicitly *not* the client mount (`_functions.py:870-889`).
        assert_eq!(ids, vec!["st-9", "im-123", "vo-1"]);
    }

    #[test]
    fn a_cpu_only_spec_still_carries_an_empty_gpu_config_and_no_placement() {
        let mut spec = sample_spec();
        spec.gpu = None;
        spec.region = None;
        let function = spec
            .function_definition("im-1", "mo-client", Vec::new(), Vec::new())
            .expect("valid spec");

        let gpu = function
            .resources
            .expect("resources")
            .gpu_config
            .expect("gpu_config");
        assert_eq!(gpu.gpu_type, "");
        assert_eq!(gpu.count, 0);
        assert!(function.scheduler_placement.is_none());
        assert_eq!(function.secret_ids, Vec::<String>::new());
        // Only the image remains a dependency.
        assert_eq!(function.object_dependencies.len(), 1);
    }

    #[test]
    fn debug_never_reveals_secret_values() {
        let mut spec = sample_spec();
        spec.image
            .env
            .insert("PLAIN_ENV".to_owned(), "env-value".to_owned());
        let rendered = format!("{spec:?}");

        assert!(rendered.contains("HF_TOKEN"), "keys stay visible");
        assert!(rendered.contains("PLAIN_ENV"), "env keys stay visible");
        assert!(
            !rendered.contains("hf_supersecret"),
            "secret value leaked into Debug"
        );
        assert!(
            !rendered.contains("env-value"),
            "image env value leaked into Debug"
        );
    }
}
