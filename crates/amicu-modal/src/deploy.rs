//! Deploying an autoscaling HTTP server to Modal.
//!
//! Every field of the `Function` message below is cited against
//! `modal-client` v1.3.1 (`proto/VENDORED.md`), because a non-Python client
//! has no way to test-drive the server's validation offline. Paths are
//! relative to the upstream checkout at that tag.

use std::collections::{BTreeMap, HashMap};

use crate::client::ModalClient;
use crate::error::ModalError;
use crate::image::{IMAGE_BUILDER_VERSION, ImageSpec};
use crate::pb::modal_client as pb;
use crate::shim::{SHIM_CLASS_NAME, SHIM_MODULE_NAME};

/// Modal's proxy regions for the server's public endpoint.
///
/// `app.py:1219` defaults `proxy_regions` to `["us-east"]` and
/// `_server.py:33-34` rejects an empty list, so this is not optional.
const PROXY_REGIONS: [&str; 1] = ["us-east"];

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
/// `Debug` redacts [`ServerSpec::secret_env`] values.
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
    /// The port the process in [`ServerSpec::argv`] listens on.
    pub port: u16,
    /// The command the container runs, verbatim; never shell-interpreted.
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
    /// Seconds Modal waits for the port to accept connections.
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
    /// Builds the `Function` message, mirroring what
    /// `app._experimental_server(...)` (`app.py:1192-1338`) feeds to
    /// `_Function.from_local` (`_functions.py:660-1180`) for a class-based
    /// web server.
    fn function_definition(
        &self,
        image_id: &str,
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
            // (`_functions.py:1120-1125`, `_resources.py:10-18`).
            .unwrap_or_default();

        Ok(pb::Function {
            // `FunctionInfo.module_name` is the *file stem* of the module the
            // class lives in (`_utils/function_utils.py:208-210`); ours is
            // baked into the image at `/root/amicu_server.py`.
            module_name: SHIM_MODULE_NAME.to_owned(),
            // `app.py:1295` passes `name_override=user_cls.__name__`, which
            // becomes `function_name` (`_utils/function_utils.py:172`). The
            // container splits this to find the class and explicitly allows
            // the bare `ClassName` form for servers
            // (`_runtime/user_code_imports.py:524-532`).
            function_name: SHIM_CLASS_NAME.to_owned(),
            // `f"{user_cls.__name__}.*"` for a class service function
            // (`_utils/function_utils.py:163-165`).
            implementation_name: format!("{SHIM_CLASS_NAME}.*"),
            // H1: empty. The Python client always ships two mounts here — its
            // own package mount and the entrypoint source file
            // (`_functions.py:730-733`, `:978`). amicu bakes the module into
            // the image instead and installs `modal` there, so there is
            // nothing to upload. See the crate docs for the fallback.
            mount_ids: Vec::new(),
            image_id: image_id.to_owned(),
            secret_ids,
            // Not serialized (no cloudpickle), so FILE
            // (`_utils/function_utils.py:141-145`).
            definition_type: pb::function::DefinitionType::File as i32,
            // `get_function_type(is_generator=None)`
            // (`_utils/function_utils.py:116-117`).
            function_type: pb::function::FunctionType::Function as i32,
            // Empty but *set*. `method_definitions` is populated only from
            // `_PartialFunctionFlags.interface_flags()` — CALLABLE_INTERFACE
            // and WEB_INTERFACE (`_partial_function.py:35-36, 63-65`) — which
            // `@modal.enter`/`@modal.exit` are not: those are lifecycle flags
            // (`_partial_function.py:32-34, 55-61`). So a server class whose
            // only decorated methods are enter/exit yields `{}` here
            // (`_functions.py:837-866`), with `method_definitions_set=True`
            // unconditionally (`_functions.py:1002`).
            method_definitions: HashMap::new(),
            method_definitions_set: true,
            // `info.is_service_class()` is True when `raw_f is None`
            // (`_utils/function_utils.py:147-151`), which is the class case.
            is_class: true,
            // `bool(info.user_cls) and not info.is_service_class()`
            // (`_functions.py:1035`) → False for the service class itself.
            is_method: false,
            // `info.class_parameter_info()` with no annotated parameters
            // (`_utils/function_utils.py:292-301`).
            class_parameter_info: Some(pb::ClassParameterInfo {
                format: pb::class_parameter_info::ParameterSerializationFormat::ParamSerializationFormatPickle as i32,
                schema: Vec::new(),
            }),
            // `app.py:1327`.
            is_server: true,
            // `app.py:1264-1270`. Note `target_concurrency` is *not* part of
            // HTTPConfig in v1.3.1 even though the message has such a field:
            // the decorator routes it to `target_concurrent_inputs` below
            // (`app.py:1315`).
            http_config: Some(pb::HttpConfig {
                port: u32::from(self.port),
                proxy_regions: PROXY_REGIONS.iter().map(|r| (*r).to_owned()).collect(),
                startup_timeout: self.startup_timeout_secs,
                // `app.py:1216` default.
                exit_grace_period: 0,
                // `app.py:1222` default.
                h2_enabled: false,
                target_concurrency: 0,
            }),
            // `_functions.py:1024` (`target_concurrent_inputs or 0`).
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
            // `autoscaler_settings` (`_functions.py:1050-1055`).
            warm_pool_size: self.min_containers,
            concurrency_limit: self.max_containers.unwrap_or(0),
            task_idle_timeout_secs: self.scaledown_window_secs,
            timeout_secs: self.timeout_secs,
            // `startup_timeout or timeout_secs` (`_functions.py:1013`).
            startup_timeout_secs: if self.startup_timeout_secs == 0 {
                self.timeout_secs
            } else {
                self.startup_timeout_secs
            },
            resources: Some(pb::Resources {
                gpu_config: Some(gpu_config),
                ..pb::Resources::default()
            }),
            // `region=` becomes a SchedulerPlacement with the region list
            // (`_functions.py:760-763`); unset when no region is requested.
            scheduler_placement: self.region.as_ref().map(|region| pb::SchedulerPlacement {
                regions: vec![region.clone()],
                nonpreemptible: false,
                ..pb::SchedulerPlacement::default()
            }),
            // `_functions.py:968-975`.
            volume_mounts,
            app_name: self.app_name.clone(),
            // False: the Python client sets this from the image builder
            // version (`_functions.py:962-966`) *and* ships the client mount
            // in `mount_ids`. With no mounts, the worker must find `modal` in
            // the image, which `ImageSpec` installs
            // (`modal_proto/api.proto:1633-1635`).
            mount_client_dependencies: false,
            // Hard-coded True by the client (`_functions.py:1058`).
            experimental_concurrent_cancellations: true,
            ..pb::Function::default()
        })
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
                // `CLIENT_TYPE_LIBMODAL` is not a Python client, but AppStopSource
                // has no libmodal variant; `modal app stop` sends CLI
                // (`cli/app.py`), which is the closest honest match.
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
    /// `ImageGetOrCreate` (+ `ImageJoinStreaming` until built) →
    /// `VolumeGetOrCreate` per volume → `SecretGetOrCreate` (only when
    /// `secret_env` is non-empty) → `FunctionPrecreate` → `FunctionCreate` →
    /// `AppPublish`.
    ///
    /// `on_build_log` receives image build output line-fragments as Modal
    /// streams them.
    pub async fn deploy_server(
        &mut self,
        spec: &ServerSpec,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<Deployment, ModalError> {
        let app_id = match self.find_app(&spec.app_name).await? {
            Some(app_id) => app_id,
            None => self.create_app(&spec.app_name).await?,
        };

        let image_id = self.build_image(&app_id, &spec.image, on_build_log).await?;

        let mut volume_mounts = Vec::with_capacity(spec.volumes.len());
        for (name, mount_path) in &spec.volumes {
            volume_mounts.push(pb::VolumeMount {
                volume_id: self.get_or_create_volume(name).await?,
                mount_path: mount_path.clone(),
                // `_functions.py:971`.
                allow_background_commits: true,
                read_only: false,
            });
        }

        let secret_ids = if spec.secret_env.is_empty() {
            Vec::new()
        } else {
            vec![self.create_app_secret(&app_id, &spec.secret_env).await?]
        };

        let function = spec.function_definition(&image_id, secret_ids, volume_mounts)?;

        // Preassign the Function's id so `FunctionCreate` updates it in place
        // rather than creating a second one (`_functions.py:1158-1178`,
        // `runner.py:152-171`).
        let precreated = self
            .stub()
            .function_precreate(pb::FunctionPrecreateRequest {
                app_id: app_id.clone(),
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
                app_id: app_id.clone(),
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
        // (`app.py:1334`).
        self.stub()
            .app_publish(pb::AppPublishRequest {
                app_id: app_id.clone(),
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
            app_id,
            function_id: created.function_id,
            url,
        })
    }

    /// `AppCreate` in the INITIALIZING state, as `modal deploy` does before
    /// publishing (`runner.py:76-99`).
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
    /// an app is being loaded (`secret.py:207-215`). The values travel over
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
    /// mirroring `image.py:654-700` and `_image_await_build_result`
    /// (`image.py:371-400`).
    async fn build_image(
        &mut self,
        app_id: &str,
        spec: &ImageSpec,
        on_build_log: &mut dyn FnMut(&str),
    ) -> Result<String, ModalError> {
        let response = self
            .stub()
            .image_get_or_create(pb::ImageGetOrCreateRequest {
                app_id: app_id.to_owned(),
                image: Some(spec.to_proto()),
                namespace: pb::DeploymentNamespace::Workspace as i32,
                builder_version: IMAGE_BUILDER_VERSION.to_owned(),
                ..pb::ImageGetOrCreateRequest::default()
            })
            .await
            .map_err(|status| ModalError::from_status("ImageGetOrCreate", status))?
            .into_inner();

        let image_id = response.image_id;
        // A non-zero status means the image is already built (`image.py:672`).
        let mut result = response.result.filter(|r| r.status != 0);

        let mut last_entry_id = String::new();
        while result.is_none() {
            let mut stream = self
                .stub()
                .image_join_streaming(pb::ImageJoinStreamingRequest {
                    image_id: image_id.clone(),
                    timeout: 55.0,
                    last_entry_id: last_entry_id.clone(),
                    include_logs_for_finished: false,
                })
                .await
                .map_err(|status| ModalError::from_status("ImageJoinStreaming", status))?
                .into_inner();

            while let Some(message) = stream
                .message()
                .await
                .map_err(|status| ModalError::from_status("ImageJoinStreaming", status))?
            {
                if !message.entry_id.is_empty() {
                    last_entry_id = message.entry_id;
                }
                for log in &message.task_logs {
                    if !log.data.is_empty() {
                        on_build_log(&log.data);
                    }
                }
                if let Some(r) = message.result
                    && r.status != 0
                {
                    result = Some(r);
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shim::render_shim;

    fn sample_spec() -> ServerSpec {
        let argv = ["vllm", "serve", "tiny"]
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        ServerSpec {
            app_name: "amicu-test".to_owned(),
            gpu: Some("H100:2".to_owned()),
            region: Some("us-east".to_owned()),
            port: 8000,
            image: ImageSpec {
                base_registry_image: "vllm/vllm-openai:v0.11.0".to_owned(),
                pip_packages: Vec::new(),
                env: BTreeMap::new(),
                shim_source: render_shim(&argv, 8000),
            },
            argv,
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
    fn function_definition_matches_the_python_client_shape() {
        let spec = sample_spec();
        let function = spec
            .function_definition(
                "im-123",
                vec!["st-9".to_owned()],
                vec![pb::VolumeMount {
                    volume_id: "vo-1".to_owned(),
                    mount_path: "/root/.cache/huggingface".to_owned(),
                    allow_background_commits: true,
                    read_only: false,
                }],
            )
            .expect("valid spec");

        assert_eq!(function.module_name, "amicu_server");
        assert_eq!(function.function_name, "Server");
        assert_eq!(function.implementation_name, "Server.*");
        assert!(function.mount_ids.is_empty());
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
        assert!(!function.mount_client_dependencies);
        assert!(function.experimental_concurrent_cancellations);
        assert!(function.function_serialized.is_empty());
        assert!(function.class_serialized.is_empty());
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
    fn a_cpu_only_spec_still_carries_an_empty_gpu_config_and_no_placement() {
        let mut spec = sample_spec();
        spec.gpu = None;
        spec.region = None;
        let function = spec
            .function_definition("im-1", Vec::new(), Vec::new())
            .expect("valid spec");

        let gpu = function
            .resources
            .expect("resources")
            .gpu_config
            .expect("gpu_config");
        assert_eq!(gpu.gpu_type, "");
        assert_eq!(gpu.count, 0);
        assert!(function.scheduler_placement.is_none());
        assert!(function.secret_ids.is_empty());
    }

    #[test]
    fn debug_never_reveals_secret_values() {
        let rendered = format!("{:?}", sample_spec());
        assert!(rendered.contains("HF_TOKEN"), "keys stay visible");
        assert!(
            !rendered.contains("hf_supersecret"),
            "secret value leaked into Debug"
        );
    }
}
