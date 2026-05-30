use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use clap::{Parser, Subcommand};
use serde::Deserialize;
use shim_core::{RunSpec, ShimError};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Output, Stdio};
use std::thread;
use std::time::Duration;

mod client;
mod crd_check;
mod llm_creds;
mod portforward;
mod render_sandbox;

#[derive(Parser, Debug)]
#[command(
    name = "void-shim-k8s",
    version,
    about = "Kubernetes integration shim for Void-Box"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MountKvmOverride {
    Auto,
    True,
    False,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum MountStrategy {
    HostPath,
    EmptyDir,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum BackendArg {
    Auto,
    Job,
    Sandbox,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum EndpointMode {
    Auto,
    InCluster,
    PortForward,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Render a Kubernetes Job manifest to stdout (MVP workflow).
    Render {
        /// Path to RunSpec YAML
        #[arg(long)]
        file: PathBuf,
        /// Kubernetes namespace
        #[arg(long, default_value = "default")]
        namespace: String,
        /// Job name prefix
        #[arg(long, default_value = "void-run")]
        name_prefix: String,
        /// Container image containing `void-box`
        #[arg(long, default_value = "ghcr.io/the-void-ia/void-box:latest")]
        image: String,
        /// Kubernetes image pull policy (Always|IfNotPresent|Never)
        #[arg(long, default_value = "IfNotPresent")]
        image_pull_policy: String,
        /// Force-override KVM auto-derivation. Default `auto` derives from sandbox.mode.
        #[arg(long, value_enum, default_value_t = MountKvmOverride::Auto)]
        mount_kvm_override: MountKvmOverride,
        /// How sandbox.mounts are materialized in the pod.
        #[arg(long, value_enum, default_value_t = MountStrategy::HostPath)]
        mount_strategy: MountStrategy,
        /// Command executed after writing /spec/run.yaml.
        #[arg(long, default_value = "voidbox run --file /spec/run.yaml")]
        command: String,
        /// Force backend selection. `auto` derives from spec.
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
    },

    /// Apply a Job to the cluster and print run_ref (namespace/job_name).
    Run {
        /// Path to RunSpec YAML
        #[arg(long)]
        file: PathBuf,
        /// Kubernetes namespace
        #[arg(long, default_value = "default")]
        namespace: String,
        /// Job name prefix
        #[arg(long, default_value = "void-run")]
        name_prefix: String,
        /// Container image containing `void-box`
        #[arg(long, default_value = "ghcr.io/the-void-ia/void-box:latest")]
        image: String,
        /// Kubernetes image pull policy (Always|IfNotPresent|Never)
        #[arg(long, default_value = "IfNotPresent")]
        image_pull_policy: String,
        /// Force-override KVM auto-derivation. Default `auto` derives from sandbox.mode.
        #[arg(long, value_enum, default_value_t = MountKvmOverride::Auto)]
        mount_kvm_override: MountKvmOverride,
        /// How sandbox.mounts are materialized in the pod.
        #[arg(long, value_enum, default_value_t = MountStrategy::HostPath)]
        mount_strategy: MountStrategy,
        /// Command executed after writing /spec/run.yaml.
        #[arg(long, default_value = "voidbox run --file /spec/run.yaml")]
        command: String,
        /// Force backend selection. `auto` derives from spec.
        #[arg(long, value_enum, default_value_t = BackendArg::Auto)]
        backend: BackendArg,
    },

    /// Get status of a run_ref in the form namespace/job_name.
    Status { run_ref: String },

    /// Fetch logs from the Job's first Pod.
    Logs {
        run_ref: String,
        #[arg(long)]
        follow: bool,
    },

    /// Remove a run by deleting the backing Job or Sandbox CR.
    Rm {
        run_ref: String,
        #[arg(long)]
        force: bool,
    },

    /// Send a message to a running service-mode agent.
    Send {
        run_ref: String,
        #[arg(long, conflicts_with = "message_file")]
        message: Option<String>,
        #[arg(long, conflicts_with = "message")]
        message_file: Option<PathBuf>,
        #[arg(long, default_value = "user")]
        role: String,
        #[arg(long, value_enum, default_value_t = EndpointMode::Auto)]
        endpoint_mode: EndpointMode,
    },

    /// Cancel a running service-mode agent.
    Cancel {
        run_ref: String,
        #[arg(long, value_enum, default_value_t = EndpointMode::Auto)]
        endpoint_mode: EndpointMode,
    },

    /// Fetch a telemetry snapshot from a running service-mode agent.
    Telemetry {
        run_ref: String,
        #[arg(long, value_enum, default_value_t = EndpointMode::Auto)]
        endpoint_mode: EndpointMode,
    },

    /// Print connection info (URL, token, sample curl) for a sandbox run_ref.
    Endpoint {
        run_ref: String,
        #[arg(long, value_enum, default_value_t = EndpointMode::Auto)]
        endpoint_mode: EndpointMode,
    },

    /// Suspend a sandbox by scaling its replicas to 0.
    Suspend { run_ref: String },

    /// Resume a sandbox by scaling its replicas to 1.
    Resume { run_ref: String },
}

fn main() -> Result<(), ShimError> {
    let cli = Cli::parse();

    match cli.cmd {
        Command::Render {
            file,
            namespace,
            name_prefix,
            image,
            image_pull_policy,
            mount_kvm_override,
            mount_strategy,
            command,
            backend,
        } => {
            let spec = shim_core::load_spec_for_shim(&file)?;
            let run_id = uuid::Uuid::new_v4();
            let name_part = normalize_dns1123_label(&spec.name, 30);
            let job_name = format!(
                "{}-{}-{}",
                normalize_dns1123_label(&name_prefix, 20),
                name_part,
                short(run_id)
            );

            let chosen = resolve_backend_choice(backend, &spec)?;
            match chosen {
                shim_core::Backend::Job => {
                    let opts = RenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm_override,
                        mount_strategy,
                        runtime_command: command,
                    };
                    let yaml = render_job_yaml(&namespace, &job_name, &opts, &spec, run_id)?;
                    print!("{}", yaml);
                }
                shim_core::Backend::Sandbox => {
                    let opts = render_sandbox::SandboxRenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm: derive_kvm_for_sandbox(mount_kvm_override, &spec),
                        runtime_command_args: vec!["serve-and-load".into()],
                        llm_credential: llm_creds::resolve_llm_credential(&spec, None),
                    };
                    let token = render_sandbox::generate_token();
                    let yaml = render_sandbox::render_sandbox_yaml(
                        &namespace, &job_name, &opts, &spec, run_id, &token,
                    )?;
                    print!("{}", yaml);
                }
            }
            Ok(())
        }
        Command::Run {
            file,
            namespace,
            name_prefix,
            image,
            image_pull_policy,
            mount_kvm_override,
            mount_strategy,
            command,
            backend,
        } => {
            let spec = shim_core::load_spec_for_shim(&file)?;
            let run_id = uuid::Uuid::new_v4();
            let name_part = normalize_dns1123_label(&spec.name, 30);
            let job_name = format!(
                "{}-{}-{}",
                normalize_dns1123_label(&name_prefix, 20),
                name_part,
                short(run_id)
            );

            let chosen = resolve_backend_choice(backend, &spec)?;
            match chosen {
                shim_core::Backend::Job => {
                    let opts = RenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm_override,
                        mount_strategy,
                        runtime_command: command,
                    };
                    let yaml = render_job_yaml(&namespace, &job_name, &opts, &spec, run_id)?;
                    kubectl_apply_yaml(&yaml)?;
                    println!("{namespace}/{job_name}");
                }
                shim_core::Backend::Sandbox => {
                    crd_check::check_sandbox_crd_installed()?;
                    let opts = render_sandbox::SandboxRenderOptions {
                        image,
                        image_pull_policy,
                        mount_kvm: derive_kvm_for_sandbox(mount_kvm_override, &spec),
                        runtime_command_args: vec!["serve-and-load".into()],
                        llm_credential: llm_creds::resolve_llm_credential(&spec, None),
                    };
                    let token = render_sandbox::generate_token();
                    let yaml = render_sandbox::render_sandbox_yaml(
                        &namespace, &job_name, &opts, &spec, run_id, &token,
                    )?;
                    kubectl_apply_yaml(&yaml)?;
                    println!("{namespace}/{job_name}");
                }
            }
            Ok(())
        }
        Command::Status { run_ref } => {
            let (namespace, name) = parse_run_ref(&run_ref)?;
            // Detect backend by querying both resources; Sandbox first.
            if sandbox_exists(&namespace, &name) {
                let status = get_sandbox_status(&namespace, &name)?;
                println!("{status}");
            } else {
                let status = get_job_status(&namespace, &name)?;
                println!("{status}");
            }
            Ok(())
        }
        Command::Logs { run_ref, follow } => {
            let (namespace, name) = parse_run_ref(&run_ref)?;
            // For Sandbox: pod has the same name as the CR. For Job: find pod
            // by job-name label.
            let pod = if sandbox_exists(&namespace, &name) {
                name.clone()
            } else {
                wait_for_job_pod(&namespace, &name, 10, Duration::from_secs(1))?
            };
            let args = build_logs_args(&namespace, &pod, follow);
            kubectl_stream(&args)
        }
        Command::Rm { run_ref, force } => {
            let (namespace, name) = parse_run_ref(&run_ref)?;
            let sandbox_args = vec![
                "delete".to_string(),
                "sandbox".to_string(),
                name.clone(),
                "-n".to_string(),
                namespace.clone(),
            ];
            if kubectl_output_owned(&sandbox_args).is_ok() {
                return Ok(());
            }
            let args = build_delete_job_args(&namespace, &name, force);
            let _ = kubectl_output_owned(&args)?;
            Ok(())
        }
        Command::Send {
            run_ref,
            message,
            message_file,
            role,
            endpoint_mode,
        } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let content = match (message, message_file) {
                (Some(m), _) => m,
                (None, Some(p)) => std::fs::read_to_string(&p)?,
                (None, None) => {
                    return Err(ShimError::CommandFailed(
                        "--message or --message-file required".into(),
                    ));
                }
            };
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) =
                resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let dc = client::DaemonClient::new(endpoint, token, 30)?;
            let voidbox_run_id = dc.first_run_id()?;
            let body = dc.send_message(&voidbox_run_id, &role, &content)?;
            println!("{body}");
            Ok(())
        }
        Command::Cancel {
            run_ref,
            endpoint_mode,
        } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) =
                resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let dc = client::DaemonClient::new(endpoint, token, 30)?;
            let voidbox_run_id = dc.first_run_id()?;
            let body = dc.cancel(&voidbox_run_id)?;
            println!("{body}");
            Ok(())
        }
        Command::Telemetry {
            run_ref,
            endpoint_mode,
        } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, _pf) =
                resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            let dc = client::DaemonClient::new(endpoint, token, 30)?;
            let voidbox_run_id = dc.first_run_id()?;
            let body = dc.telemetry(&voidbox_run_id)?;
            println!("{body}");
            Ok(())
        }
        Command::Endpoint {
            run_ref,
            endpoint_mode,
        } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let token = read_sandbox_token(&namespace, &sandbox_name)?;
            let (endpoint, pf) =
                resolve_sandbox_endpoint(&namespace, &sandbox_name, endpoint_mode)?;
            println!("URL: {endpoint}");
            println!("Token: {token}");
            println!("Sample curl: curl -H 'Authorization: Bearer {token}' {endpoint}/v1/runs");
            if pf.is_some() {
                println!(
                    "(port-forward will close when this command exits; press Ctrl-C to terminate)"
                );
                std::thread::park();
            }
            Ok(())
        }
        Command::Suspend { run_ref } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let target = format!("sandbox/{sandbox_name}");
            let args = ["scale", "-n", &namespace, &target, "--replicas=0"];
            kubectl_output(&args)?;
            println!("Suspended {run_ref}");
            Ok(())
        }
        Command::Resume { run_ref } => {
            let (namespace, sandbox_name) = parse_run_ref(&run_ref)?;
            let target = format!("sandbox/{sandbox_name}");
            let args = ["scale", "-n", &namespace, &target, "--replicas=1"];
            kubectl_output(&args)?;
            println!("Resumed {run_ref}");
            Ok(())
        }
    }
}

#[derive(Debug, Clone)]
struct RenderOptions {
    image: String,
    image_pull_policy: String,
    mount_kvm_override: MountKvmOverride,
    mount_strategy: MountStrategy,
    runtime_command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunState {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown,
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RunState::Pending => "Pending",
            RunState::Running => "Running",
            RunState::Succeeded => "Succeeded",
            RunState::Failed => "Failed",
            RunState::Unknown => "Unknown",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Deserialize)]
struct JobResource {
    status: Option<JobStatus>,
}

#[derive(Debug, Deserialize)]
struct JobStatus {
    active: Option<u32>,
    succeeded: Option<u32>,
    failed: Option<u32>,
    conditions: Option<Vec<JobCondition>>,
}

#[derive(Debug, Deserialize)]
struct JobCondition {
    #[serde(rename = "type")]
    type_name: String,
    status: String,
}

fn short(id: uuid::Uuid) -> String {
    let s = id.to_string();
    s.split('-').next().unwrap_or("run").to_string()
}

fn parse_run_ref(run_ref: &str) -> Result<(String, String), ShimError> {
    let mut parts = run_ref.split('/');
    let namespace = parts
        .next()
        .ok_or_else(|| ShimError::InvalidRunRef("missing namespace".to_string()))?;
    let job_name = parts
        .next()
        .ok_or_else(|| ShimError::InvalidRunRef("missing job name".to_string()))?;

    if parts.next().is_some() || namespace.is_empty() || job_name.is_empty() {
        return Err(ShimError::InvalidRunRef(
            "run_ref must match namespace/job_name".to_string(),
        ));
    }

    if !is_k8s_name(namespace) || !is_k8s_name(job_name) {
        return Err(ShimError::InvalidRunRef(
            "run_ref contains invalid Kubernetes name characters".to_string(),
        ));
    }

    Ok((namespace.to_string(), job_name.to_string()))
}

fn is_k8s_name(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }

    let bytes = value.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }

    // DNS-1123 label: lowercase alphanumerics + dash only. Both namespace
    // and pod/job names are DNS-1123 labels (NOT subdomains), so '.' is invalid.
    value
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn get_job_status(namespace: &str, job_name: &str) -> Result<RunState, ShimError> {
    let args = ["get", "job", job_name, "-n", namespace, "-o", "json"];
    let output = kubectl_output(&args)?;
    let job: JobResource = serde_json::from_slice(&output.stdout)
        .map_err(|e| ShimError::CommandFailed(format!("failed to parse job json: {e}")))?;
    Ok(map_job_to_state(job.status.as_ref()))
}

/// Returns true if a Sandbox CR with the given name exists in the namespace.
fn sandbox_exists(namespace: &str, name: &str) -> bool {
    let args = ["get", "sandbox", name, "-n", namespace, "-o", "name"];
    kubectl_output(&args).is_ok()
}

/// Reads the Ready condition of a Sandbox CR and maps it to [`RunState`].
fn get_sandbox_status(namespace: &str, name: &str) -> Result<RunState, ShimError> {
    let args = [
        "get",
        "sandbox",
        name,
        "-n",
        namespace,
        "-o",
        "jsonpath={.status.conditions[?(@.type==\"Ready\")].status}",
    ];
    let output = kubectl_output(&args)?;
    let ready = String::from_utf8_lossy(&output.stdout).trim().to_string();
    match ready.as_str() {
        "True" => Ok(RunState::Running),
        "False" => Ok(RunState::Pending),
        _ => Ok(RunState::Unknown),
    }
}

/// Validates the user's `--backend` choice against the spec's detected backend.
///
/// - `auto` → detected backend (no error, no warning).
/// - explicit + matches → use it.
/// - `--backend sandbox` over non-service spec → fatal `BackendMismatch`.
/// - `--backend job` over service spec → stderr warning, returns Job.
fn resolve_backend_choice(
    arg: BackendArg,
    spec: &shim_core::RunSpec,
) -> Result<shim_core::Backend, ShimError> {
    let detected = shim_core::detect_backend(spec);
    match (arg, detected) {
        (BackendArg::Auto, b) => Ok(b),
        (BackendArg::Job, _) => {
            if detected == shim_core::Backend::Sandbox {
                eprintln!(
                    "WARN: rendering 'mode: service' spec as Job — service semantics \
                     (send/telemetry/cancel routing) will not work; the daemon will run \
                     but no client routing exists. Pass --backend auto or sandbox instead."
                );
            }
            Ok(shim_core::Backend::Job)
        }
        (BackendArg::Sandbox, shim_core::Backend::Sandbox) => Ok(shim_core::Backend::Sandbox),
        (BackendArg::Sandbox, shim_core::Backend::Job) => Err(ShimError::BackendMismatch(
            spec.name.clone(),
            "non-service spec".into(),
            "sandbox backend requires kind:agent + mode:service".into(),
        )),
    }
}

fn map_job_to_state(status: Option<&JobStatus>) -> RunState {
    let Some(status) = status else {
        return RunState::Pending;
    };

    if status.succeeded.unwrap_or(0) > 0 {
        return RunState::Succeeded;
    }
    if status.failed.unwrap_or(0) > 0 {
        return RunState::Failed;
    }
    if status.active.unwrap_or(0) > 0 {
        return RunState::Running;
    }

    if let Some(conditions) = &status.conditions {
        for condition in conditions {
            if condition.status != "True" {
                continue;
            }
            if condition.type_name == "Complete" {
                return RunState::Succeeded;
            }
            if condition.type_name == "Failed" {
                return RunState::Failed;
            }
        }
    }

    RunState::Unknown
}

fn wait_for_job_pod(
    namespace: &str,
    job_name: &str,
    retries: usize,
    delay: Duration,
) -> Result<String, ShimError> {
    let args = [
        "get",
        "pods",
        "-n",
        namespace,
        "-l",
        &format!("job-name={job_name}"),
        "-o",
        "jsonpath={.items[0].metadata.name}",
    ];

    for _ in 0..retries {
        let output = kubectl_output(&args)?;
        let pod = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !pod.is_empty() {
            return Ok(pod);
        }
        thread::sleep(delay);
    }

    Err(ShimError::CommandFailed(format!(
        "no pod found for job {namespace}/{job_name}"
    )))
}

fn build_logs_args(namespace: &str, pod: &str, follow: bool) -> Vec<String> {
    let mut args = vec![
        "logs".to_string(),
        "-n".to_string(),
        namespace.to_string(),
        pod.to_string(),
    ];
    if follow {
        args.push("-f".to_string());
    }
    args
}

fn build_delete_job_args(namespace: &str, job_name: &str, force: bool) -> Vec<String> {
    let mut args = vec![
        "delete".to_string(),
        "job".to_string(),
        job_name.to_string(),
        "-n".to_string(),
        namespace.to_string(),
    ];
    if force {
        args.push("--force".to_string());
        args.push("--grace-period=0".to_string());
    }
    args
}

fn kubectl_apply_yaml(yaml: &str) -> Result<(), ShimError> {
    let mut child = ProcessCommand::new("kubectl")
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(map_spawn_error)?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(yaml.as_bytes()).map_err(|e| {
            ShimError::CommandFailed(format!("failed to write manifest to kubectl stdin: {e}"))
        })?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| ShimError::CommandFailed(format!("failed waiting for kubectl: {e}")))?;

    if output.status.success() {
        Ok(())
    } else {
        Err(command_failure("kubectl apply", &output))
    }
}

fn kubectl_output(args: &[&str]) -> Result<Output, ShimError> {
    let output = ProcessCommand::new("kubectl")
        .args(args)
        .output()
        .map_err(map_spawn_error)?;

    if output.status.success() {
        Ok(output)
    } else {
        Err(command_failure(
            &format!("kubectl {}", args.join(" ")),
            &output,
        ))
    }
}

fn kubectl_output_owned(args: &[String]) -> Result<Output, ShimError> {
    let refs = args.iter().map(String::as_str).collect::<Vec<_>>();
    kubectl_output(&refs)
}

fn kubectl_stream(args: &[String]) -> Result<(), ShimError> {
    let status = ProcessCommand::new("kubectl")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(map_spawn_error)?;

    if status.success() {
        Ok(())
    } else {
        Err(ShimError::CommandFailed(format!(
            "kubectl {} failed with status {}",
            args.join(" "),
            status
        )))
    }
}

fn map_spawn_error(err: std::io::Error) -> ShimError {
    if err.kind() == std::io::ErrorKind::NotFound {
        ShimError::CommandNotFound("kubectl".to_string())
    } else {
        ShimError::Io(err)
    }
}

fn command_failure(command: &str, output: &Output) -> ShimError {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if stderr.is_empty() {
        ShimError::CommandFailed(format!("{command} failed with status {}", output.status))
    } else {
        ShimError::CommandFailed(format!(
            "{command} failed with status {}: {stderr}",
            output.status
        ))
    }
}

fn render_job_yaml(
    namespace: &str,
    job_name: &str,
    options: &RenderOptions,
    spec: &RunSpec,
    run_id: uuid::Uuid,
) -> Result<String, ShimError> {
    validate_yaml_scalar(namespace, "namespace")?;
    validate_yaml_scalar(job_name, "job_name")?;
    validate_yaml_scalar(&options.image, "image")?;
    validate_yaml_scalar(&options.image_pull_policy, "image_pull_policy")?;
    validate_yaml_scalar(&options.runtime_command, "command")?;

    let run_yaml = serde_yaml::to_string(spec)?;
    let run_yaml_b64 = BASE64_STANDARD.encode(run_yaml.as_bytes());

    let mount_kvm = match options.mount_kvm_override {
        MountKvmOverride::Auto => derive_kvm_from_mode(&spec.sandbox.mode),
        MountKvmOverride::True => true,
        MountKvmOverride::False => false,
    };
    let privileged = mount_kvm;

    let (volumes_yaml, volume_mounts_yaml) =
        render_volumes_for_mounts(&spec.sandbox.mounts, options.mount_strategy, mount_kvm)?;

    let privileged_yaml = if privileged {
        "          securityContext:\n            privileged: true\n"
    } else {
        ""
    };

    let manifest = format!(
        r#"apiVersion: batch/v1
kind: Job
metadata:
  name: {job_name}
  namespace: {namespace}
  labels:
    app: void-shim-k8s
    void.run/id: "{run_id}"
spec:
  backoffLimit: 0
  template:
    metadata:
      labels:
        app: void-shim-k8s
        void.run/id: "{run_id}"
    spec:
      restartPolicy: Never
      containers:
        - name: void-box
          image: {image}
          imagePullPolicy: {image_pull_policy}
{privileged_yaml}          command: ["/bin/sh", "-lc"]
          args:
            - |
              set -eu
              mkdir -p /spec
              printf '%s' '{run_yaml_b64}' | base64 -d > /spec/run.yaml
              {runtime_command}
{volume_mounts_yaml}{volumes_yaml}"#,
        job_name = job_name,
        namespace = namespace,
        image = options.image,
        image_pull_policy = options.image_pull_policy,
        run_id = run_id,
        runtime_command = options.runtime_command,
        run_yaml_b64 = run_yaml_b64,
        privileged_yaml = privileged_yaml,
        volume_mounts_yaml = volume_mounts_yaml,
        volumes_yaml = volumes_yaml,
    );
    Ok(manifest)
}

fn derive_kvm_from_mode(mode: &str) -> bool {
    // void-box sandbox.mode values: auto | mock | kvm | vz
    matches!(mode, "auto" | "kvm")
}

fn derive_kvm_for_sandbox(override_: MountKvmOverride, spec: &shim_core::RunSpec) -> bool {
    match override_ {
        MountKvmOverride::Auto => derive_kvm_from_mode(&spec.sandbox.mode),
        MountKvmOverride::True => true,
        MountKvmOverride::False => false,
    }
}

fn read_sandbox_token(namespace: &str, sandbox_name: &str) -> Result<String, ShimError> {
    let secret_name = format!("{sandbox_name}-token");
    let args = [
        "get",
        "secret",
        &secret_name,
        "-n",
        namespace,
        "-o",
        "jsonpath={.data.token}",
    ];
    let output = kubectl_output(&args)?;
    let b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let bytes = BASE64_STANDARD
        .decode(&b64)
        .map_err(|e| ShimError::CommandFailed(format!("decode token: {e}")))?;
    String::from_utf8(bytes).map_err(|e| ShimError::CommandFailed(format!("token not utf8: {e}")))
}

fn resolve_sandbox_endpoint(
    namespace: &str,
    sandbox_name: &str,
    mode: EndpointMode,
) -> Result<(String, Option<portforward::PortForward>), ShimError> {
    let effective = match mode {
        EndpointMode::Auto => {
            if std::env::var("KUBERNETES_SERVICE_HOST").is_ok() {
                EndpointMode::InCluster
            } else {
                EndpointMode::PortForward
            }
        }
        other => other,
    };
    match effective {
        EndpointMode::InCluster => {
            let url = format!("http://{sandbox_name}.{namespace}.svc.cluster.local:43100");
            Ok((url, None))
        }
        EndpointMode::PortForward => {
            let pf = portforward::PortForward::open(namespace, sandbox_name, 43100)?;
            let url = pf.url();
            Ok((url, Some(pf)))
        }
        EndpointMode::Auto => unreachable!("Auto was resolved above"),
    }
}

fn render_volumes_for_mounts(
    mounts: &[shim_core::MountSpec],
    strategy: MountStrategy,
    include_kvm: bool,
) -> Result<(String, String), ShimError> {
    let mut volumes = String::new();
    let mut volume_mounts = String::new();

    let has_any = include_kvm || !mounts.is_empty();
    if !has_any {
        return Ok((String::new(), String::new()));
    }

    volume_mounts.push_str("          volumeMounts:\n");
    volumes.push_str("\n      volumes:\n");

    if include_kvm {
        volume_mounts.push_str("            - name: devkvm\n              mountPath: /dev/kvm\n");
        volumes.push_str("        - name: devkvm\n          hostPath:\n            path: /dev/kvm\n            type: CharDevice\n");
    }

    for (i, m) in mounts.iter().enumerate() {
        validate_mount_host(&m.host)?;
        let name = format!("mount-{i}");
        let read_only = m.mode == "ro";
        volume_mounts.push_str(&format!(
            "            - name: {name}\n              mountPath: {path}\n              readOnly: {ro}\n",
            name = name, path = m.host, ro = read_only
        ));
        match strategy {
            MountStrategy::HostPath => volumes.push_str(&format!(
                "        - name: {name}\n          hostPath:\n            path: {path}\n            type: DirectoryOrCreate\n",
                name = name, path = m.host
            )),
            MountStrategy::EmptyDir => volumes.push_str(&format!(
                "        - name: {name}\n          emptyDir: {{}}\n",
                name = name
            )),
        }
    }

    Ok((volumes, volume_mounts))
}

fn validate_mount_host(host: &str) -> Result<(), ShimError> {
    if !host.starts_with('/') {
        return Err(ShimError::InvalidMount(format!(
            "mount host path must be absolute, got: {host}"
        )));
    }
    if host.contains('\n') || host.contains('\r') {
        return Err(ShimError::InvalidMount(format!(
            "mount host path must not contain newlines, got: {host:?}"
        )));
    }
    Ok(())
}

fn validate_yaml_scalar(value: &str, field: &str) -> Result<(), ShimError> {
    if value.contains('\n') || value.contains('\r') {
        return Err(ShimError::InvalidScalar(
            field.to_string(),
            format!("must not contain newlines, got: {value:?}"),
        ));
    }
    Ok(())
}

fn normalize_dns1123_label(s: &str, max_len: usize) -> String {
    let mut out: String = s
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if out.len() > max_len {
        out.truncate(max_len);
    }
    out = out.trim_matches('-').to_string();
    if out.is_empty() {
        out = "x".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_run_ref_accepts_valid_values() {
        let (ns, name) = parse_run_ref("default/void-run-123").expect("run_ref should parse");
        assert_eq!(ns, "default");
        assert_eq!(name, "void-run-123");
    }

    #[test]
    fn parse_run_ref_rejects_invalid_values() {
        assert!(parse_run_ref("justname").is_err());
        assert!(parse_run_ref("default/").is_err());
        assert!(parse_run_ref("Default/name").is_err());
    }

    #[test]
    fn map_job_status_prefers_succeeded_failed_active() {
        let status = JobStatus {
            active: Some(1),
            succeeded: Some(0),
            failed: Some(0),
            conditions: None,
        };
        assert_eq!(map_job_to_state(Some(&status)), RunState::Running);

        let status = JobStatus {
            active: Some(0),
            succeeded: Some(1),
            failed: Some(0),
            conditions: None,
        };
        assert_eq!(map_job_to_state(Some(&status)), RunState::Succeeded);

        let status = JobStatus {
            active: Some(0),
            succeeded: Some(0),
            failed: Some(1),
            conditions: None,
        };
        assert_eq!(map_job_to_state(Some(&status)), RunState::Failed);
    }

    #[test]
    fn map_job_status_uses_conditions_when_counts_absent() {
        let status = JobStatus {
            active: None,
            succeeded: None,
            failed: None,
            conditions: Some(vec![JobCondition {
                type_name: "Complete".to_string(),
                status: "True".to_string(),
            }]),
        };
        assert_eq!(map_job_to_state(Some(&status)), RunState::Succeeded);
    }

    #[test]
    fn delete_args_include_force_only_when_requested() {
        let normal = build_delete_job_args("default", "job1", false);
        assert_eq!(
            normal,
            vec!["delete", "job", "job1", "-n", "default"]
                .into_iter()
                .map(str::to_string)
                .collect::<Vec<_>>()
        );

        let forced = build_delete_job_args("default", "job1", true);
        assert!(forced.contains(&"--force".to_string()));
        assert!(forced.contains(&"--grace-period=0".to_string()));
    }

    #[test]
    fn logs_args_include_follow_flag() {
        let args = build_logs_args("default", "pod-123", true);
        assert_eq!(args.last().expect("args should not be empty"), "-f");
    }

    fn sample_agent_spec(mode: &str) -> shim_core::RunSpec {
        let yaml = format!(
            r#"
api_version: v1
kind: agent
name: smoke
sandbox: {{ mode: {mode}, memory_mb: 256, vcpus: 1 }}
agent: {{ prompt: "hi", timeout_secs: 5 }}
"#
        );
        shim_core::parse_spec_str(&yaml).expect("sample spec parses")
    }

    fn default_opts() -> RenderOptions {
        RenderOptions {
            image: "ghcr.io/the-void-ia/void-box:latest".to_string(),
            image_pull_policy: "IfNotPresent".to_string(),
            mount_kvm_override: MountKvmOverride::Auto,
            mount_strategy: MountStrategy::HostPath,
            runtime_command: "voidbox run --file /spec/run.yaml".to_string(),
        }
    }

    #[test]
    fn render_includes_kvm_for_mode_kvm() {
        let spec = sample_agent_spec("kvm");
        let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect("render ok");
        assert!(
            yaml.contains("/dev/kvm"),
            "expected /dev/kvm mount; got:\n{yaml}"
        );
        assert!(
            yaml.contains("privileged: true"),
            "expected privileged: true; got:\n{yaml}"
        );
    }

    #[test]
    fn render_includes_kvm_for_mode_auto() {
        let spec = sample_agent_spec("auto");
        let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect("render ok");
        assert!(
            yaml.contains("/dev/kvm"),
            "expected /dev/kvm for mode=auto; got:\n{yaml}"
        );
        assert!(
            yaml.contains("privileged: true"),
            "expected privileged for mode=auto; got:\n{yaml}"
        );
    }

    #[test]
    fn render_rejects_namespace_with_newline() {
        let spec = sample_agent_spec("mock");
        let err = render_job_yaml("ns\ninject", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect_err("newline in namespace should fail");
        assert!(matches!(err, ShimError::InvalidScalar(_, _)), "got {err:?}");
    }

    #[test]
    fn render_rejects_command_with_newline() {
        let spec = sample_agent_spec("mock");
        let mut opts = default_opts();
        opts.runtime_command = "voidbox run\necho gotcha".into();
        let err = render_job_yaml("default", "j", &opts, &spec, uuid::Uuid::nil())
            .expect_err("newline in command should fail");
        assert!(matches!(err, ShimError::InvalidScalar(_, _)), "got {err:?}");
    }

    #[test]
    fn render_excludes_kvm_for_mode_mock() {
        let spec = sample_agent_spec("mock");
        let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect("render ok");
        assert!(
            !yaml.contains("/dev/kvm"),
            "expected no /dev/kvm; got:\n{yaml}"
        );
        assert!(
            !yaml.contains("privileged: true"),
            "expected no privileged; got:\n{yaml}"
        );
    }

    #[test]
    fn render_emits_volumes_for_sandbox_mounts() {
        let mut spec = sample_agent_spec("mock");
        spec.sandbox.mounts = vec![
            shim_core::MountSpec {
                host: "/tmp/a".into(),
                guest: "/a".into(),
                mode: "ro".into(),
            },
            shim_core::MountSpec {
                host: "/tmp/b".into(),
                guest: "/b".into(),
                mode: "rw".into(),
            },
        ];
        let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect("render ok");
        assert!(
            yaml.contains("name: mount-0"),
            "mount-0 missing in:\n{yaml}"
        );
        assert!(
            yaml.contains("name: mount-1"),
            "mount-1 missing in:\n{yaml}"
        );
        assert!(yaml.contains("path: /tmp/a"), "/tmp/a missing in:\n{yaml}");
        assert!(
            yaml.contains("readOnly: true"),
            "readOnly true missing in:\n{yaml}"
        );
        assert!(
            yaml.contains("readOnly: false"),
            "readOnly false missing in:\n{yaml}"
        );
    }

    #[test]
    fn render_rejects_relative_host_path() {
        let mut spec = sample_agent_spec("mock");
        spec.sandbox.mounts = vec![shim_core::MountSpec {
            host: "rel/path".into(),
            guest: "/a".into(),
            mode: "ro".into(),
        }];
        let err = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect_err("relative path should fail");
        assert!(matches!(err, ShimError::InvalidMount(_)), "got {err:?}");
    }

    #[test]
    fn render_includes_voidbox_command_default() {
        let spec = sample_agent_spec("mock");
        let yaml = render_job_yaml("default", "j", &default_opts(), &spec, uuid::Uuid::nil())
            .expect("render ok");
        assert!(
            yaml.contains("voidbox run --file /spec/run.yaml"),
            "default command missing or wrong; got:\n{yaml}"
        );
    }

    #[test]
    fn normalize_dns1123_strips_invalid_chars() {
        assert_eq!(normalize_dns1123_label("My Run!", 30), "my-run");
        assert_eq!(normalize_dns1123_label("ABC_123", 30), "abc-123");
        assert_eq!(normalize_dns1123_label("---x---", 30), "x");
        assert_eq!(normalize_dns1123_label("", 30), "x");
    }
}
